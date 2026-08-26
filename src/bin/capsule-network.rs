//! Rootless, outbound-only networking for an Internet-only capsule.
//!
//! The trusted parent creates a child in fresh user, mount and network
//! namespaces. `slirp4netns` attaches a userspace TCP/IP stack to that network
//! namespace while nftables blocks host-loopback, link-local, private, LAN and
//! other non-public destinations. Sandwine then immediately enters another,
//! nested user namespace and Bubblewrap drops capabilities before starting the
//! application, so it has no authority over the parent network namespace or
//! its filter.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::mount::{MountFlags, MountPropagationFlags, mount_bind, mount_change, mount_remount};
use rustix::process::{
    Pid, Signal, WaitOptions, getgid, getpid, getppid, getuid, kill_process,
    set_parent_process_death_signal, waitpid,
};
use rustix::thread::{UnshareFlags, unshare_unsafe};

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const RESOLV_CONF: &[u8] = b"nameserver 10.0.2.3\noptions edns0 trust-ad\n";
const PING_GROUP_RANGE_PATH: &str = "/proc/sys/net/ipv4/ping_group_range";
const PRIVATE_PING_GROUP_RANGE: &[u8] = b"0 0\n";

// Keep each range explicit so review and tests can verify the effective
// boundary. The DNS exception is limited to slirp's synthetic resolver before
// all of 10/8 is rejected. Loopback is private to this network namespace.
const INTERNET_ONLY_NFT_RULES: &str = r#"table inet capsule {
    chain output {
        type filter hook output priority filter; policy accept;
        oifname "lo" accept
        ip daddr 10.0.2.3 udp dport 53 accept
        ip daddr 10.0.2.3 tcp dport 53 accept
        ip daddr 0.0.0.0/8 reject
        ip daddr 10.0.0.0/8 reject
        ip daddr 100.64.0.0/10 reject
        ip daddr 127.0.0.0/8 reject
        ip daddr 169.254.0.0/16 reject
        ip daddr 172.16.0.0/12 reject
        ip daddr 192.0.0.0/24 reject
        ip daddr 192.0.2.0/24 reject
        ip daddr 192.31.196.0/24 reject
        ip daddr 192.52.193.0/24 reject
        ip daddr 192.88.99.0/24 reject
        ip daddr 192.168.0.0/16 reject
        ip daddr 192.175.48.0/24 reject
        ip daddr 198.18.0.0/15 reject
        ip daddr 198.51.100.0/24 reject
        ip daddr 203.0.113.0/24 reject
        ip daddr 224.0.0.0/4 reject
        ip daddr 240.0.0.0/4 reject
        meta nfproto ipv6 oifname != "lo" reject
    }
}
"#;

#[derive(Debug, Eq, PartialEq)]
struct Arguments {
    slirp4netns: PathBuf,
    nft: PathBuf,
    command: Vec<OsString>,
}

fn main() {
    let result = match parse_arguments(env::args_os().skip(1)) {
        Ok((true, arguments)) => run_network_child(arguments),
        Ok((false, arguments)) => run_supervisor(arguments),
        Err(error) => Err(error),
    };
    match result {
        Ok(status) => exit_with_status(status),
        Err(error) => {
            eprintln!("capsule-network failed: {error:#}");
            std::process::exit(125);
        }
    }
}

fn parse_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<(bool, Arguments)> {
    let mut arguments = arguments.into_iter().peekable();
    let child = arguments
        .next_if(|argument| argument == "--child")
        .is_some();
    expect_option(&mut arguments, "--slirp4netns")?;
    let slirp4netns = absolute_executable(arguments.next(), "slirp4netns")?;
    expect_option(&mut arguments, "--nft")?;
    let nft = absolute_executable(arguments.next(), "nft")?;
    expect_option(&mut arguments, "--")?;
    let command: Vec<_> = arguments.collect();
    if command.is_empty() {
        bail!("no contained command was supplied");
    }
    Ok((
        child,
        Arguments {
            slirp4netns,
            nft,
            command,
        },
    ))
}

fn expect_option(
    arguments: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
    expected: &str,
) -> Result<()> {
    match arguments.next() {
        Some(argument) if argument == expected => Ok(()),
        _ => bail!("usage: capsule-network --slirp4netns PATH --nft PATH -- COMMAND [ARG ...]"),
    }
}

fn absolute_executable(candidate: Option<OsString>, name: &str) -> Result<PathBuf> {
    let path = PathBuf::from(candidate.ok_or_else(|| anyhow!("missing {name} path"))?);
    if !path.is_absolute() {
        bail!("{name} path is not absolute: {path:?}");
    }
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("cannot inspect {name} executable {path:?}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("{name} path is not an executable regular file: {path:?}");
    }
    Ok(path)
}

fn run_supervisor(arguments: Arguments) -> Result<ExitStatus> {
    let current_executable = env::current_exe().context("cannot locate capsule-network")?;
    let mut child_arguments = vec![
        OsString::from("--child"),
        OsString::from("--slirp4netns"),
        arguments.slirp4netns.as_os_str().to_os_string(),
        OsString::from("--nft"),
        arguments.nft.as_os_str().to_os_string(),
        OsString::from("--"),
    ];
    child_arguments.extend(arguments.command);

    let mut network_child = Command::new(current_executable)
        .args(child_arguments)
        .spawn()
        .context("cannot start network-namespace child")?;
    let child_pid = Pid::from_raw(network_child.id() as i32)
        .ok_or_else(|| anyhow!("network child has an invalid process ID"))?;

    let status = waitpid(Some(child_pid), WaitOptions::UNTRACED)
        .context("cannot wait for network-namespace setup")?
        .ok_or_else(|| anyhow!("network child did not report setup state"))?
        .1;
    if !status.stopped() || status.stopping_signal() != Some(Signal::STOP.as_raw()) {
        bail!("network child exited before its namespace was ready: {status:?}");
    }

    let mut slirp = match start_slirp(&arguments.slirp4netns, child_pid) {
        Ok(slirp) => slirp,
        Err(error) => {
            terminate_and_reap(&mut network_child, child_pid);
            return Err(error);
        }
    };

    if let Err(error) = kill_process(child_pid, Signal::CONT) {
        terminate_and_reap(&mut network_child, child_pid);
        stop_slirp(&mut slirp);
        return Err(error).context("cannot release configured network child");
    }

    let status = network_child
        .wait()
        .context("cannot wait for contained command")?;
    stop_slirp(&mut slirp);
    Ok(status)
}

fn start_slirp(path: &Path, child_pid: Pid) -> Result<Child> {
    let (mut ready_reader, ready_writer) =
        UnixStream::pair().context("cannot create slirp4netns readiness socket")?;
    fcntl_setfd(&ready_writer, FdFlags::empty())
        .context("cannot pass slirp4netns readiness descriptor")?;
    ready_reader
        .set_read_timeout(Some(READY_TIMEOUT))
        .context("cannot configure slirp4netns readiness timeout")?;
    let ready_fd = ready_writer.as_raw_fd().to_string();
    let ready_argument = format!("--ready-fd={ready_fd}");
    let pid_argument = child_pid.as_raw_nonzero().get().to_string();
    let mut child = Command::new(path)
        .args([
            OsStr::new("--configure"),
            OsStr::new("--disable-host-loopback"),
            OsStr::new("--enable-sandbox"),
            OsStr::new("--enable-seccomp"),
            OsStr::new("--exit-fd=0"),
            OsStr::new(&ready_argument),
            OsStr::new(&pid_argument),
            OsStr::new("tap0"),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start rootless network helper {path:?}"))?;
    drop(ready_writer);

    // Keep the write end open for the run. slirp4netns watches its stdin and
    // exits automatically if Capsule dies and the descriptor is closed.
    let exit_guard = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("slirp4netns exit descriptor was not created"))?;
    let mut byte = [0_u8; 1];
    match ready_reader.read_exact(&mut byte) {
        Ok(()) if byte[0] == b'1' => {
            // ChildStdin is deliberately restored to the process object as a
            // lifetime guard. `stop_slirp` drops it before waiting.
            child.stdin = Some(exit_guard);
            Ok(child)
        }
        Ok(()) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "slirp4netns returned an invalid readiness byte: {}",
                byte[0]
            )
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error).context("slirp4netns did not configure the network within 15 seconds")
        }
    }
}

fn stop_slirp(child: &mut Child) {
    drop(child.stdin.take());
    if child.wait().is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn terminate_and_reap(child: &mut Child, pid: Pid) {
    let _ = kill_process(pid, Signal::KILL);
    let _ = child.wait();
}

fn run_network_child(arguments: Arguments) -> Result<ExitStatus> {
    let expected_parent = getppid().ok_or_else(|| anyhow!("network supervisor is missing"))?;
    set_parent_process_death_signal(Some(Signal::KILL))
        .context("cannot bind network child to its supervisor")?;
    if getppid() != Some(expected_parent) {
        bail!("network supervisor exited during child setup");
    }

    create_namespaces().context("cannot create private network namespace")?;
    enable_private_ping_sockets().context("cannot enable ping in private network namespace")?;
    install_private_resolver().context("cannot install private DNS configuration")?;

    // The supervisor attaches slirp to this exact namespace while the child
    // is stopped, leaving no interval in which the game can run unfiltered.
    kill_process(getpid(), Signal::STOP).context("cannot pause network child for attachment")?;

    install_firewall(&arguments.nft).context("cannot install Internet-only firewall")?;

    let mut command = Command::new(&arguments.command[0]);
    command.args(&arguments.command[1..]);
    let error = command.exec();
    Err(error).context("cannot execute contained sandbox")
}

fn create_namespaces() -> Result<()> {
    let outer_uid = getuid().as_raw();
    let outer_gid = getgid().as_raw();
    // SAFETY: capsule-network is a dedicated, single-threaded helper at this
    // point. It has created no threads before this child-only call.
    unsafe { unshare_unsafe(UnshareFlags::NEWUSER | UnshareFlags::NEWNET | UnshareFlags::NEWNS) }
        .context("unshare(CLONE_NEWUSER|CLONE_NEWNET|CLONE_NEWNS) failed")?;

    match std::fs::write("/proc/self/setgroups", "deny\n") {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot deny setgroups in user namespace"),
    }
    std::fs::write("/proc/self/uid_map", format!("0 {outer_uid} 1\n"))
        .context("cannot write user-namespace uid_map")?;
    std::fs::write("/proc/self/gid_map", format!("0 {outer_gid} 1\n"))
        .context("cannot write user-namespace gid_map")?;

    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .context("cannot make private mount propagation")?;
    Ok(())
}

fn enable_private_ping_sockets() -> Result<()> {
    // Sandwine creates a nested user namespace and maps its contained root to
    // group 0 in this namespace. Linux checks ICMP datagram sockets against
    // the network namespace's ping_group_range, so the inherited host value
    // (commonly 65534 65534) otherwise makes UnityEngine.Ping and Wine's
    // IcmpSendEcho fail even while TCP, UDP and slirp DNS are working.
    //
    // This sysctl is network-namespace-local. It gives the contained identity
    // ping sockets only; it grants neither raw sockets nor host/LAN access,
    // and the nftables destination policy still applies to every echo packet.
    std::fs::write(PING_GROUP_RANGE_PATH, PRIVATE_PING_GROUP_RANGE)
        .context("cannot write private ping_group_range")?;
    Ok(())
}

fn install_private_resolver() -> Result<()> {
    // A named temporary inode is needed because Linux cannot bind-mount an
    // anonymous memfd on every supported filesystem. It is created mode 0600
    // and unlinked as soon as the private bind mount holds a reference.
    let mut resolver =
        tempfile::NamedTempFile::new_in("/tmp").context("cannot create private resolver file")?;
    resolver
        .write_all(RESOLV_CONF)
        .context("cannot write private resolver file")?;
    mount_bind(resolver.path(), "/etc/resolv.conf").context("cannot bind private resolver file")?;
    mount_remount(
        "/etc/resolv.conf",
        MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
        "",
    )
    .context("cannot make private resolver file read-only")?;
    drop(resolver);
    Ok(())
}

fn install_firewall(nft: &Path) -> Result<()> {
    let mut child = Command::new(nft)
        .args(["--check", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start nftables checker {nft:?}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("nftables checker stdin was not created"))?
        .write_all(INTERNET_ONLY_NFT_RULES.as_bytes())
        .context("cannot send rules to nftables checker")?;
    let checked = child.wait().context("cannot wait for nftables checker")?;
    if !checked.success() {
        bail!("nftables rejected the Internet-only rules: {checked}");
    }

    let mut child = Command::new(nft)
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start nftables loader {nft:?}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("nftables loader stdin was not created"))?
        .write_all(INTERNET_ONLY_NFT_RULES.as_bytes())
        .context("cannot send rules to nftables loader")?;
    let loaded = child.wait().context("cannot wait for nftables loader")?;
    if !loaded.success() {
        bail!("nftables could not enforce the Internet-only rules: {loaded}");
    }
    Ok(())
}

fn exit_with_status(status: ExitStatus) -> ! {
    if let Some(code) = status.code() {
        std::process::exit(code);
    }
    std::process::exit(128 + status.signal().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_absolute_trusted_tool_paths() {
        let (_, arguments) = parse_arguments([
            OsString::from("--slirp4netns"),
            OsString::from("/bin/true"),
            OsString::from("--nft"),
            OsString::from("/bin/true"),
            OsString::from("--"),
            OsString::from("game"),
            OsString::from("two words"),
        ])
        .unwrap();
        assert_eq!(arguments.command, ["game", "two words"]);

        assert!(
            parse_arguments([
                OsString::from("--slirp4netns"),
                OsString::from("relative"),
                OsString::from("--nft"),
                OsString::from("/bin/true"),
                OsString::from("--"),
                OsString::from("game"),
            ])
            .is_err()
        );
    }

    #[test]
    fn policy_blocks_every_private_ipv4_class_and_non_loopback_ipv6() {
        for range in [
            "10.0.0.0/8",
            "100.64.0.0/10",
            "127.0.0.0/8",
            "169.254.0.0/16",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "224.0.0.0/4",
            "240.0.0.0/4",
        ] {
            assert!(INTERNET_ONLY_NFT_RULES.contains(&format!("ip daddr {range} reject")));
        }
        assert!(INTERNET_ONLY_NFT_RULES.contains("meta nfproto ipv6 oifname != \"lo\" reject"));
        assert!(INTERNET_ONLY_NFT_RULES.contains("ip daddr 10.0.2.3 udp dport 53 accept"));
        assert!(!INTERNET_ONLY_NFT_RULES.contains("--api-socket"));
    }

    #[test]
    fn nested_sandbox_root_is_the_only_private_ping_group() {
        assert_eq!(PRIVATE_PING_GROUP_RANGE, b"0 0\n");
        assert_eq!(PING_GROUP_RANGE_PATH, "/proc/sys/net/ipv4/ping_group_range");
    }
}
