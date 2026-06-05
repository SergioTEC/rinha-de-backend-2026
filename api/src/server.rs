// API server using epoll. Receives client TCP file descriptors from the LB
// via recvmsg(SCM_RIGHTS) and serves HTTP/1.1 over them directly.
//
// Threads:
//   * fd-accept: accept LB control connections on the UDS listener.
//   * fd-recv (per control conn): blocking recvmsg loop, pushes FDs onto
//     a channel and pokes an eventfd.
//   * epoll (main thread): waits on client FDs + eventfd, parses HTTP,
//     runs the IVF classifier, writes the response.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc;
use std::sync::Arc;

use crate::http::{FraudResult, handle_connection};
use crate::AppState;

const READ_BUF_SIZE: usize = 8192;
const MAX_EVENTS: usize = 64;
const LISTEN_BACKLOG: i32 = 4096;
const WAKE_TOKEN: u64 = u64::MAX;

struct Conn {
    fd: RawFd,
    read_buf: Vec<u8>,
    filled: usize,
}

impl Conn {
    fn new(fd: RawFd) -> Self {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            let one: libc::c_int = 1;
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                &one as *const _ as *const _,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_QUICKACK,
                &one as *const _ as *const _,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        Conn {
            fd,
            read_buf: vec![0u8; READ_BUF_SIZE],
            filled: 0,
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

pub fn run(sock_path: &str, get_state: Arc<dyn Fn() -> Option<Arc<AppState>> + Send + Sync>) -> io::Result<()> {
    let _ = std::fs::remove_file(sock_path);
    let listener_fd = bind_uds_listener(sock_path)?;
    eprintln!("[server] listening on {}", sock_path);

    let (fd_tx, fd_rx) = mpsc::channel::<RawFd>();
    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if wake_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let tx = fd_tx.clone();
    std::thread::Builder::new()
        .name("fd-accept".into())
        .spawn(move || accept_loop(listener_fd, tx, wake_fd))?;

    epoll_main_loop(get_state, fd_rx, wake_fd)
}

fn bind_uds_listener(path: &str) -> io::Result<RawFd> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let cpath = CString::new(path).unwrap();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let p = cpath.as_bytes_with_nul();
    if p.len() > addr.sun_path.len() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
    }
    for (i, &b) in p.iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let path_len = (std::mem::size_of_val(&addr.sun_family) + p.len()) as libc::socklen_t;
    if unsafe { libc::bind(fd, &addr as *const _ as *const _, path_len) } < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    unsafe { libc::chmod(cpath.as_ptr(), 0o666) };
    if unsafe { libc::listen(fd, LISTEN_BACKLOG) } < 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

fn accept_loop(listener_fd: RawFd, fd_tx: mpsc::Sender<RawFd>, wake_fd: RawFd) {
    loop {
        let control = unsafe {
            libc::accept4(
                listener_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if control < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("[server] accept error: {}", err);
            continue;
        }
        let tx = fd_tx.clone();
        std::thread::Builder::new()
            .name("fd-recv".into())
            .spawn(move || recv_loop(control, tx, wake_fd))
            .ok();
    }
}

fn recv_loop(control_fd: RawFd, fd_tx: mpsc::Sender<RawFd>, wake_fd: RawFd) {
    while let Some(client_fd) = recv_fd(control_fd) {
        if fd_tx.send(client_fd).is_err() {
            unsafe { libc::close(client_fd) };
            break;
        }
        let one: u64 = 1;
        unsafe { libc::write(wake_fd, &one as *const _ as *const _, 8) };
    }
    unsafe { libc::close(control_fd) };
}

fn recv_fd(control_fd: RawFd) -> Option<RawFd> {
    let mut payload: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: &mut payload as *mut _ as *mut _,
        iov_len: 1,
    };
    let mut cmsg_buf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_buf.len() as _;
    loop {
        let n = unsafe { libc::recvmsg(control_fd, &mut msg, 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if n == 0 {
            return None;
        }
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET
                    && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                    && (*cmsg).cmsg_len
                        >= libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _
                {
                    let mut fd: libc::c_int = -1;
                    std::ptr::copy_nonoverlapping(
                        libc::CMSG_DATA(cmsg) as *const u8,
                        &mut fd as *mut libc::c_int as *mut u8,
                        std::mem::size_of::<libc::c_int>(),
                    );
                    return Some(fd);
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
    }
}

fn epoll_main_loop(
    get_state: Arc<dyn Fn() -> Option<Arc<AppState>> + Send + Sync>,
    fd_rx: mpsc::Receiver<RawFd>,
    wake_fd: RawFd,
) -> io::Result<()> {
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: WAKE_TOKEN,
        };
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, wake_fd, &mut ev);
    }
    let mut conns: HashMap<RawFd, Conn> = HashMap::with_capacity(2048);
    let mut events: Vec<libc::epoll_event> =
        vec![libc::epoll_event { events: 0, u64: 0 }; MAX_EVENTS];
    loop {
        let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), MAX_EVENTS as i32, -1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            eprintln!("[server] epoll_wait error: {}", err);
            break;
        }
        for i in 0..n as usize {
            let ev = events[i];
            let token = ev.u64;
            if token == WAKE_TOKEN {
                let mut drain = [0u8; 8];
                while unsafe { libc::read(wake_fd, drain.as_mut_ptr() as *mut _, 8) } == 8 {}
                while let Ok(client_fd) = fd_rx.try_recv() {
                    register_client(epfd, client_fd, &mut conns);
                }
            } else {
                handle_client_event(&ev, epfd, &mut conns, get_state.as_ref());
            }
        }
    }
    unsafe { libc::close(epfd) };
    Ok(())
}

fn register_client(epfd: RawFd, client_fd: RawFd, conns: &mut HashMap<RawFd, Conn>) {
    let conn = Conn::new(client_fd);
    unsafe {
        let mut e = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: client_fd as u64,
        };
        if libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, client_fd, &mut e) < 0 {
            eprintln!("[server] epoll add client failed: {}", io::Error::last_os_error());
            return;
        }
    }
    conns.insert(client_fd, conn);
}

fn handle_client_event(
    ev: &libc::epoll_event,
    epfd: RawFd,
    conns: &mut HashMap<RawFd, Conn>,
    get_state: &(dyn Fn() -> Option<Arc<AppState>> + Send + Sync),
) {
    let fd = ev.u64 as RawFd;
    let close_now = {
        let Some(c) = conns.get_mut(&fd) else {
            return;
        };
        if ev.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
            drop_conn(conns, fd, epfd);
            return;
        }
        if ev.events & libc::EPOLLIN as u32 != 0 {
            let res = handle_readable(c, get_state);
            if !res {
                drop_conn(conns, fd, epfd);
                return;
            }
        }
        false
    };
    if close_now {
        drop_conn(conns, fd, epfd);
    }
}

fn handle_readable(
    c: &mut Conn,
    get_state: &(dyn Fn() -> Option<Arc<AppState>> + Send + Sync),
) -> bool {
    loop {
        if c.filled >= c.read_buf.len() {
            return false;
        }
        let n = unsafe {
            libc::recv(
                c.fd,
                c.read_buf[c.filled..].as_mut_ptr() as *mut _,
                c.read_buf.len() - c.filled,
                0,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                break;
            }
            return false;
        }
        if n == 0 {
            return false;
        }
        c.filled += n as usize;
    }
    let resp = process_buffer(c, get_state);
    if let Some(bytes) = resp {
        let mut pos = 0;
        while pos < bytes.len() {
            let n = unsafe {
                libc::send(
                    c.fd,
                    bytes[pos..].as_ptr() as *const _,
                    bytes.len() - pos,
                    libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EAGAIN) || err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    return true;
                }
                return false;
            }
            pos += n as usize;
        }
        unsafe {
            let one: libc::c_int = 1;
            libc::setsockopt(
                c.fd,
                libc::IPPROTO_TCP,
                libc::TCP_QUICKACK,
                &one as *const _ as *const _,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        return false;
    }
    true
}

fn process_buffer(
    c: &mut Conn,
    get_state: &(dyn Fn() -> Option<Arc<AppState>> + Send + Sync),
) -> Option<Vec<u8>> {
    use crate::http::{
        get_content_length, parse_http_request, HTTP_BAD_REQUEST, HTTP_NOT_FOUND, HTTP_READY,
    };
    use crate::process;

    const SCORE_RESPONSES: [&[u8]; 6] = [
        b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}",
        b"HTTP/1.1 200 OK\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}",
    ];
    const SERVICE_UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 33\r\nConnection: keep-alive\r\n\r\n{\"error\":\"not ready, still loading\"}";

    let buf = &c.read_buf[..c.filled];
    let (method, path, body_start) = match parse_http_request(buf) {
        Some(v) => v,
        None => return None,
    };
    let body_len = get_content_length(&buf[..body_start]);
    let total_len = body_start + body_len;
    if c.filled < total_len {
        return None;
    }
    let body = &buf[body_start..total_len];

    let response: Vec<u8>;
    if method == "GET" && path == "/ready" {
        // Only return 200 when STATE is set (dataset loaded).
        if get_state().is_some() {
            response = HTTP_READY.to_vec();
        } else {
            response = SERVICE_UNAVAILABLE.to_vec();
        }
    } else if method == "POST" && path == "/fraud-score" {
        match get_state().as_ref() {
            Some(state) => match process(body, state) {
                FraudResult::Score(n) => response = SCORE_RESPONSES[n.min(5)].to_vec(),
                FraudResult::Error => response = HTTP_BAD_REQUEST.to_vec(),
            },
            None => response = SERVICE_UNAVAILABLE.to_vec(),
        }
    } else {
        response = HTTP_NOT_FOUND.to_vec();
    }

    if c.filled > total_len {
        c.read_buf.copy_within(total_len..c.filled, 0);
        c.filled -= total_len;
    } else {
        c.filled = 0;
    }
    Some(response)
}

fn drop_conn(conns: &mut HashMap<RawFd, Conn>, fd: RawFd, epfd: RawFd) {
    if conns.remove(&fd).is_some() {
        unsafe {
            let mut ev = libc::epoll_event {
                events: 0,
                u64: 0,
            };
            let _ = libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, &mut ev);
        }
    }
}
