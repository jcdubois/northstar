use crate::{
    common::container::Container,
    debug, info,
    npk::manifest::{Capability, RLimitResource, RLimitValue},
    runtime::{
        fork::util::{self, fork, set_child_subreaper, set_log_target, set_process_name},
        ipc::{owned_fd::OwnedFd, Message as IpcMessage},
        ExitStatus, Pid,
    },
    seccomp::AllowList,
};
pub use builder::build;
use itertools::Itertools;
use nix::{
    errno::Errno,
    libc::{self, c_ulong},
    mount::MsFlags,
    sched::unshare,
    sys::{
        signal::Signal,
        wait::{waitpid, WaitStatus},
    },
    unistd,
    unistd::Uid,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::CString,
    os::unix::{
        net::UnixStream,
        prelude::{AsRawFd, RawFd},
    },
    path::PathBuf,
    process::exit,
};

mod builder;

// Message from the forker to init and response
#[derive(Debug, Serialize, Deserialize)]
pub enum Message {
    /// The init process forked a new child with `pid`
    Forked { pid: Pid },
    /// A child of init exited with `exit_status`
    Exit { pid: Pid, exit_status: ExitStatus },
    /// Exec a new process
    Exec {
        path: PathBuf,
        args: Vec<String>,
        env: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Init {
    pub container: Container,
    pub root: PathBuf,
    pub uid: u16,
    pub gid: u16,
    pub mounts: Vec<Mount>,
    pub groups: Vec<u32>,
    pub capabilities: Option<HashSet<Capability>>,
    pub rlimits: Option<HashMap<RLimitResource, RLimitValue>>,
    pub seccomp: Option<AllowList>,
    pub console: bool,
}

impl Init {
    pub fn run(self, mut stream: IpcMessage<UnixStream>, console: Option<OwnedFd>) -> ! {
        set_log_target(format!("northstar::init::{}", self.container));

        // Become a subreaper
        set_child_subreaper(true);

        // Set the process name to init. This process inherited the process name
        // from the runtime
        set_process_name(&format!("init-{}", self.container));

        // Become a session group leader
        debug!("Setting session id");
        unistd::setsid().expect("Failed to call setsid");

        // Enter mount namespace
        debug!("Entering mount namespace");
        unshare(nix::sched::CloneFlags::CLONE_NEWNS).expect("Failed to unshare NEWNS");

        // Perform all mounts passed in mounts
        self.mount();

        // Set the chroot to the containers root mount point
        debug!("Chrooting to {}", self.root.display());
        unistd::chroot(&self.root).expect("Failed to chroot");

        // Set current working directory to root
        debug!("Setting current working directory to root");
        env::set_current_dir("/").expect("Failed to set cwd to /");

        // UID / GID
        self.set_ids();

        // Supplementary groups
        self.set_groups();

        // Apply resource limits
        self.set_rlimits();

        // No new privileges
        Self::set_no_new_privs(true);

        // Capabilities
        self.drop_privileges();

        loop {
            match stream.recv() {
                Ok(Some(Message::Exec {
                    path,
                    args,
                    mut env,
                })) => {
                    debug!("Execing {} {}", path.display(), args.iter().join(" "));

                    // The init process got adopted by the forker after the trampoline exited. It is
                    // safe to set the parent death signal now.
                    util::set_parent_death_signal(Signal::SIGKILL);

                    if let Some(fd) = console.as_ref().map(AsRawFd::as_raw_fd) {
                        // Add the fd number to the environment of the application
                        env.push(format!("NORTHSTAR_CONSOLE={}", fd));
                    }

                    let io = stream.recv_fds::<RawFd, 3>().expect("Failed to receive io");
                    debug_assert!(io.len() == 3);
                    let stdin = io[0];
                    let stdout = io[1];
                    let stderr = io[2];

                    // Start new process inside the container
                    let pid = fork(|| {
                        set_log_target(format!("northstar::{}", self.container));
                        util::set_parent_death_signal(Signal::SIGKILL);

                        unistd::dup2(stdin, nix::libc::STDIN_FILENO).expect("Failed to dup2");
                        unistd::dup2(stdout, nix::libc::STDOUT_FILENO).expect("Failed to dup2");
                        unistd::dup2(stderr, nix::libc::STDERR_FILENO).expect("Failed to dup2");

                        unistd::close(stdin).expect("Failed to close stdout after dup2");
                        unistd::close(stdout).expect("Failed to close stdout after dup2");
                        unistd::close(stderr).expect("Failed to close stderr after dup2");

                        // Set seccomp filter
                        if let Some(ref filter) = self.seccomp {
                            filter.apply().expect("Failed to apply seccomp filter.");
                        }

                        let path = CString::new(path.to_str().unwrap()).unwrap();
                        let args = args
                            .iter()
                            .map(|s| CString::new(s.as_str()).unwrap())
                            .collect::<Vec<_>>();
                        let env = env
                            .iter()
                            .map(|s| CString::new(s.as_str()).unwrap())
                            .collect::<Vec<_>>();

                        panic!(
                            "Execve: {:?} {:?}: {:?}",
                            &path,
                            &args,
                            unistd::execve(&path, &args, &env)
                        )
                    })
                    .expect("Failed to spawn child process");

                    // close fds
                    drop(console);
                    unistd::close(stdin).expect("Failed to close stdout");
                    unistd::close(stdout).expect("Failed to close stdout");
                    unistd::close(stderr).expect("Failed to close stderr");

                    let message = Message::Forked { pid };
                    stream.send(&message).expect("Failed to send fork result");

                    // Wait for the child to exit
                    loop {
                        debug!("Waiting for child process {} to exit", pid);
                        match waitpid(Some(unistd::Pid::from_raw(pid as i32)), None) {
                            Ok(WaitStatus::Exited(_pid, status)) => {
                                debug!("Child process {} exited with status code {}", pid, status);
                                let exit_status = ExitStatus::Exit(status);
                                stream
                                    .send(Message::Exit { pid, exit_status })
                                    .expect("Channel error");

                                assert_eq!(
                                    waitpid(Some(unistd::Pid::from_raw(pid as i32)), None),
                                    Err(nix::Error::ECHILD)
                                );

                                exit(0);
                            }
                            Ok(WaitStatus::Signaled(_pid, status, _)) => {
                                debug!("Child process {} exited with signal {}", pid, status);
                                let exit_status = ExitStatus::Signalled(status as u8);
                                stream
                                    .send(Message::Exit { pid, exit_status })
                                    .expect("Channel error");

                                assert_eq!(
                                    waitpid(Some(unistd::Pid::from_raw(pid as i32)), None),
                                    Err(nix::Error::ECHILD)
                                );

                                exit(0);
                            }
                            Ok(WaitStatus::Continued(_)) | Ok(WaitStatus::Stopped(_, _)) => {
                                log::error!("Child process continued or stopped");
                                continue;
                            }
                            Err(nix::Error::EINTR) => continue,
                            e => panic!("Failed to waitpid on {}: {:?}", pid, e),
                        }
                    }
                }
                Ok(None) => {
                    info!("Channel closed. Exiting...");
                    std::process::exit(0);
                }
                Ok(_) => unimplemented!("Unimplemented message"),
                Err(e) => panic!("Failed to receive message: {}", e),
            }
        }
    }

    /// Set uid/gid
    fn set_ids(&self) {
        let uid = self.uid;
        let gid = self.gid;

        let rt_privileged = unistd::geteuid() == Uid::from_raw(0);

        // If running as uid 0 save our caps across the uid/gid drop
        if rt_privileged {
            caps::securebits::set_keepcaps(true).expect("Failed to set keep caps");
        }

        debug!("Setting resgid {}", gid);
        let gid = unistd::Gid::from_raw(gid.into());
        unistd::setresgid(gid, gid, gid).expect("Failed to set resgid");

        let uid = unistd::Uid::from_raw(uid.into());
        debug!("Setting resuid {}", uid);
        unistd::setresuid(uid, uid, uid).expect("Failed to set resuid");

        if rt_privileged {
            self.reset_effective_caps();
            caps::securebits::set_keepcaps(false).expect("Failed to set keep caps");
        }
    }

    fn set_groups(&self) {
        debug!("Setting groups {:?}", self.groups);
        let result = unsafe { nix::libc::setgroups(self.groups.len(), self.groups.as_ptr()) };

        Errno::result(result)
            .map(drop)
            .expect("Failed to set supplementary groups");
    }

    fn set_rlimits(&self) {
        if let Some(limits) = self.rlimits.as_ref() {
            debug!("Applying rlimits");
            for (resource, limit) in limits {
                let resource = match resource {
                    RLimitResource::AS => rlimit::Resource::AS,
                    RLimitResource::CORE => rlimit::Resource::CORE,
                    RLimitResource::CPU => rlimit::Resource::CPU,
                    RLimitResource::DATA => rlimit::Resource::DATA,
                    RLimitResource::FSIZE => rlimit::Resource::FSIZE,
                    RLimitResource::LOCKS => rlimit::Resource::LOCKS,
                    RLimitResource::MEMLOCK => rlimit::Resource::MEMLOCK,
                    RLimitResource::MSGQUEUE => rlimit::Resource::MSGQUEUE,
                    RLimitResource::NICE => rlimit::Resource::NICE,
                    RLimitResource::NOFILE => rlimit::Resource::NOFILE,
                    RLimitResource::NPROC => rlimit::Resource::NPROC,
                    RLimitResource::RSS => rlimit::Resource::RSS,
                    RLimitResource::RTPRIO => rlimit::Resource::RTPRIO,
                    #[cfg(not(target_os = "android"))]
                    RLimitResource::RTTIME => rlimit::Resource::RTTIME,
                    RLimitResource::SIGPENDING => rlimit::Resource::SIGPENDING,
                    RLimitResource::STACK => rlimit::Resource::STACK,
                };
                resource
                    .set(
                        limit.soft.unwrap_or(rlimit::INFINITY),
                        limit.hard.unwrap_or(rlimit::INFINITY),
                    )
                    .expect("Failed to set rlimit");
            }
        }
    }

    /// Drop capabilities
    fn drop_privileges(&self) {
        debug!("Dropping priviledges");
        let mut bounded =
            caps::read(None, caps::CapSet::Bounding).expect("Failed to read bounding caps");
        // Convert the set from the manifest to a set of caps::Capability
        let set = self
            .capabilities
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<caps::Capability>>();
        bounded.retain(|c| !set.contains(c));

        for cap in &bounded {
            // caps::set cannot be called for bounded
            caps::drop(None, caps::CapSet::Bounding, *cap).expect("Failed to drop bounding cap");
        }
        caps::set(None, caps::CapSet::Effective, &set).expect("Failed to set effective caps");
        caps::set(None, caps::CapSet::Permitted, &set).expect("Failed to set permitted caps");
        caps::set(None, caps::CapSet::Inheritable, &set).expect("Failed to set inheritable caps");
        caps::set(None, caps::CapSet::Ambient, &set).expect("Failed to set ambient caps");
    }

    // Reset effective caps to the most possible set
    fn reset_effective_caps(&self) {
        let all = caps::all();
        caps::set(None, caps::CapSet::Effective, &all).expect("Failed to reset effective caps");
    }

    /// Execute list of mount calls
    fn mount(&self) {
        for mount in &self.mounts {
            mount.mount();
        }
    }

    fn set_no_new_privs(value: bool) {
        #[cfg(target_os = "android")]
        pub const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;
        #[cfg(not(target_os = "android"))]
        use libc::PR_SET_NO_NEW_PRIVS;

        debug!("Setting no new privs");
        let result = unsafe { nix::libc::prctl(PR_SET_NO_NEW_PRIVS, value as c_ulong, 0, 0, 0) };
        Errno::result(result)
            .map(drop)
            .expect("Failed to set PR_SET_NO_NEW_PRIVS")
    }
}

impl From<Capability> for caps::Capability {
    fn from(cap: Capability) -> Self {
        match cap {
            Capability::CAP_CHOWN => caps::Capability::CAP_CHOWN,
            Capability::CAP_DAC_OVERRIDE => caps::Capability::CAP_DAC_OVERRIDE,
            Capability::CAP_DAC_READ_SEARCH => caps::Capability::CAP_DAC_READ_SEARCH,
            Capability::CAP_FOWNER => caps::Capability::CAP_FOWNER,
            Capability::CAP_FSETID => caps::Capability::CAP_FSETID,
            Capability::CAP_KILL => caps::Capability::CAP_KILL,
            Capability::CAP_SETGID => caps::Capability::CAP_SETGID,
            Capability::CAP_SETUID => caps::Capability::CAP_SETUID,
            Capability::CAP_SETPCAP => caps::Capability::CAP_SETPCAP,
            Capability::CAP_LINUX_IMMUTABLE => caps::Capability::CAP_LINUX_IMMUTABLE,
            Capability::CAP_NET_BIND_SERVICE => caps::Capability::CAP_NET_BIND_SERVICE,
            Capability::CAP_NET_BROADCAST => caps::Capability::CAP_NET_BROADCAST,
            Capability::CAP_NET_ADMIN => caps::Capability::CAP_NET_ADMIN,
            Capability::CAP_NET_RAW => caps::Capability::CAP_NET_RAW,
            Capability::CAP_IPC_LOCK => caps::Capability::CAP_IPC_LOCK,
            Capability::CAP_IPC_OWNER => caps::Capability::CAP_IPC_OWNER,
            Capability::CAP_SYS_MODULE => caps::Capability::CAP_SYS_MODULE,
            Capability::CAP_SYS_RAWIO => caps::Capability::CAP_SYS_RAWIO,
            Capability::CAP_SYS_CHROOT => caps::Capability::CAP_SYS_CHROOT,
            Capability::CAP_SYS_PTRACE => caps::Capability::CAP_SYS_PTRACE,
            Capability::CAP_SYS_PACCT => caps::Capability::CAP_SYS_PACCT,
            Capability::CAP_SYS_ADMIN => caps::Capability::CAP_SYS_ADMIN,
            Capability::CAP_SYS_BOOT => caps::Capability::CAP_SYS_BOOT,
            Capability::CAP_SYS_NICE => caps::Capability::CAP_SYS_NICE,
            Capability::CAP_SYS_RESOURCE => caps::Capability::CAP_SYS_RESOURCE,
            Capability::CAP_SYS_TIME => caps::Capability::CAP_SYS_TIME,
            Capability::CAP_SYS_TTY_CONFIG => caps::Capability::CAP_SYS_TTY_CONFIG,
            Capability::CAP_MKNOD => caps::Capability::CAP_MKNOD,
            Capability::CAP_LEASE => caps::Capability::CAP_LEASE,
            Capability::CAP_AUDIT_WRITE => caps::Capability::CAP_AUDIT_WRITE,
            Capability::CAP_AUDIT_CONTROL => caps::Capability::CAP_AUDIT_CONTROL,
            Capability::CAP_SETFCAP => caps::Capability::CAP_SETFCAP,
            Capability::CAP_MAC_OVERRIDE => caps::Capability::CAP_MAC_OVERRIDE,
            Capability::CAP_MAC_ADMIN => caps::Capability::CAP_MAC_ADMIN,
            Capability::CAP_SYSLOG => caps::Capability::CAP_SYSLOG,
            Capability::CAP_WAKE_ALARM => caps::Capability::CAP_WAKE_ALARM,
            Capability::CAP_BLOCK_SUSPEND => caps::Capability::CAP_BLOCK_SUSPEND,
            Capability::CAP_AUDIT_READ => caps::Capability::CAP_AUDIT_READ,
            Capability::CAP_PERFMON => caps::Capability::CAP_PERFMON,
            Capability::CAP_BPF => caps::Capability::CAP_BPF,
            Capability::CAP_CHECKPOINT_RESTORE => caps::Capability::CAP_CHECKPOINT_RESTORE,
        }
    }
}

/// Instructions for mount system call done in init
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mount {
    pub source: Option<PathBuf>,
    pub target: PathBuf,
    pub fstype: Option<String>,
    pub flags: u64,
    pub data: Option<String>,
    pub error_msg: String,
}

impl Mount {
    pub fn new(
        source: Option<PathBuf>,
        target: PathBuf,
        fstype: Option<&'static str>,
        flags: MsFlags,
        data: Option<String>,
    ) -> Mount {
        let error_msg = format!(
            "Failed to mount '{}' of type '{}' on '{}' with flags '{:?}' and data '{}'",
            source.clone().unwrap_or_default().display(),
            fstype.unwrap_or_default(),
            target.display(),
            flags,
            data.clone().unwrap_or_default()
        );
        Mount {
            source,
            target,
            fstype: fstype.map(|s| s.to_string()),
            flags: flags.bits(),
            data,
            error_msg,
        }
    }

    /// Execute this mount call
    pub(super) fn mount(&self) {
        nix::mount::mount(
            self.source.as_ref(),
            &self.target,
            self.fstype.as_deref(),
            // Safe because flags is private and only set in Mount::new via MsFlags::bits
            unsafe { MsFlags::from_bits_unchecked(self.flags) },
            self.data.as_deref(),
        )
        .expect(&self.error_msg);
    }
}
