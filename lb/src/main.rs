use std::env;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

fn main() {
    let port = env::var("LB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9999u16);

    let backends_env = env::var("LB_BACKENDS")
        .unwrap_or_else(|_| "/tmp/sockets/api1.sock,/tmp/sockets/api2.sock".to_string());

    let backend_paths: Vec<String> = backends_env
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    println!("[LB] Starting on port {}", port);
    println!("[LB] Backends: {:?}", backend_paths);

    // === BIND TCP NA PORTA 9999 IMEDIATAMENTE ===
    // Isso garante que o health check do bot encontra a porta aberta
    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .expect("Failed to bind TCP");
    listener.set_nonblocking(true).ok();
    println!("[LB] TCP listener bound on port {}", port);

    // === CONECTAR NOS BACKENDS EM PARALELO ===
    // Usar thread separada para não bloquear o accept loop
    let (backend_tx, backend_rx) = std::sync::mpsc::channel::<Option<UnixStream>>();
    
    for path in &backend_paths {
        let path = path.clone();
        let tx = backend_tx.clone();
        std::thread::spawn(move || {
            let stream = connect_with_retry(&path, 120, 500); // 120 retries, 500ms = 60s max
            let _ = tx.send(stream);
        });
    }

    let mut backends: Vec<Option<UnixStream>> = Vec::with_capacity(backend_paths.len());
    for _ in 0..backend_paths.len() {
        backends.push(None);
    }

    let mut next_backend = 0usize;
    let mut connected_count = 0;

    println!("[LB] Ready (accepting connections while backends warm up)");

    // === ACCEPT LOOP PRINCIPAL ===
    // Aceita conexões TCP imediatamente, repassa quando backends estiverem prontos
    for stream in listener.incoming() {
        // Verificar se novos backends conectaram (non-blocking)
        while let Ok(Some(stream)) = backend_rx.try_recv() {
            if let Some(idx) = backends.iter().position(|b| b.is_none()) {
                backends[idx] = Some(stream);
                connected_count += 1;
                println!("[LB] Backend {}/{} connected", connected_count, backends.len());
            }
        }

        match stream {
            Ok(client) => {
                // Se nenhum backend pronto, droppar silenciosamente
                // O health check do bot vai retry automaticamente
                if connected_count == 0 {
                    drop(client);
                    continue;
                }

                // Round-robin entre backends disponíveis
                let mut attempts = 0;
                let mut sent = false;
                while attempts < backends.len() {
                    let idx = next_backend;
                    next_backend = (next_backend + 1) % backends.len();
                    attempts += 1;

                    if let Some(ref mut unix) = backends[idx] {
                        let fd = client.as_raw_fd();
                        if let Err(e) = send_fd(unix, fd) {
                            // Backend morreu, marcar como offline
                            backends[idx] = None;
                            connected_count = backends.iter().filter(|b| b.is_some()).count();
                            continue;
                        }
                        sent = true;
                        break;
                    }
                }

                if !sent {
                    drop(client);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // No connections available, brief sleep
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            Err(e) => {
                eprintln!("[LB] Accept error: {}", e);
            }
        }
    }
}

fn connect_with_retry(path: &str, max_retries: usize, delay_ms: u64) -> Option<UnixStream> {
    for attempt in 0..max_retries {
        match UnixStream::connect(path) {
            Ok(stream) => {
                println!("[LB] Connected to {} (attempt {})", path, attempt + 1);
                return Some(stream);
            }
            Err(e) if attempt < max_retries - 1 => {
                if attempt % 10 == 0 {
                    println!("[LB] Retry {}/{} connecting to {}: {}", attempt + 1, max_retries, path, e);
                }
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(e) => {
                eprintln!("[LB] Failed to connect to {}: {}", path, e);
                return None;
            }
        }
    }
    None
}

fn send_fd(stream: &mut UnixStream, fd: RawFd) -> io::Result<()> {
    let sock_fd = stream.as_raw_fd();
    let dummy = [0u8; 1];

    let fd_size = std::mem::size_of::<RawFd>();
    let cmsg_space = unsafe { libc::CMSG_SPACE(fd_size as libc::c_uint) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut iov = libc::iovec {
        iov_base: dummy.as_ptr() as *mut libc::c_void,
        iov_len: dummy.len(),
    };

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::new(io::ErrorKind::Other, "CMSG_FIRSTHDR failed"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as libc::c_uint) as _;

        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        *data_ptr = fd;
    }

    let res = unsafe { libc::sendmsg(sock_fd, &msg, 0) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}
