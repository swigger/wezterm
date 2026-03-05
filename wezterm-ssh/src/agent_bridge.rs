//! Bridge between Unix domain sockets and Windows named pipes for SSH agent
//!
//! This module provides functionality to create a Unix domain socket that forwards
//! requests to the Windows SSH agent via named pipes.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket};

/// Creates a Unix socket bridge to the Windows SSH agent
/// Returns the path to the Unix socket that can be used as SSH_AUTH_SOCK
#[cfg(windows)]
pub fn create_agent_bridge() -> Result<(PathBuf, AgentBridge)> {
    use std::fs;
    use wezterm_uds::UnixListener;
    
    // Create a temporary directory for the socket
    let temp_dir = std::env::temp_dir();
    let socket_path = temp_dir.join(format!("wezterm-ssh-agent-{}.sock", std::process::id()));
    
    // Remove old socket if it exists
    let _ = fs::remove_file(&socket_path);
    
    // Create Unix domain socket listener
    let listener = UnixListener::bind(&socket_path)
        .context("Failed to create Unix domain socket for agent bridge")?;
    
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let socket_path_clone = socket_path.clone();
    
    // Spawn thread to handle connections
    std::thread::spawn(move || {
        if let Err(e) = run_bridge_server(listener, running_clone) {
            log::error!("Agent bridge server error: {:#}", e);
        }
        // Clean up socket on exit
        let _ = fs::remove_file(&socket_path_clone);
    });
    
    Ok((socket_path, AgentBridge { running }))
}

#[cfg(not(windows))]
pub fn create_agent_bridge() -> Result<(PathBuf, AgentBridge)> {
    anyhow::bail!("Agent bridge is only supported on Windows");
}

pub struct AgentBridge {
    running: Arc<AtomicBool>,
}

impl Drop for AgentBridge {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(windows)]
fn run_bridge_server(listener: wezterm_uds::UnixListener, running: Arc<AtomicBool>) -> Result<()> {
    use std::time::Duration;
    
    listener.set_nonblocking(true)
        .context("Failed to set listener to non-blocking")?;
    
    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                let running_clone = running.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_bridge_connection(stream, running_clone) {
                        log::error!("Error handling bridge connection: {:#}", e);
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::error!("Error accepting connection: {:#}", e);
                break;
            }
        }
    }
    
    Ok(())
}

#[cfg(windows)]
fn handle_bridge_connection(
    mut unix_stream: wezterm_uds::UnixStream,
    running: Arc<AtomicBool>,
) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::fs::OpenOptions;
    use winapi::um::winbase::FILE_FLAG_OVERLAPPED;
    
    // Connect to Windows named pipe
    const PIPE_NAME: &str = "\\\\.\\pipe\\openssh-ssh-agent";
    
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(PIPE_NAME)
        .context("Failed to connect to Windows SSH agent named pipe")?;
    
    unix_stream.set_nonblocking(true)?;
    
    let mut unix_buf = vec![0u8; 8192];
    let mut pipe_buf = vec![0u8; 8192];
    
    while running.load(Ordering::Relaxed) {
        // Forward from Unix socket to named pipe
        match unix_stream.read(&mut unix_buf) {
            Ok(0) => break,
            Ok(n) => {
                pipe.write_all(&unix_buf[..n])
                    .context("Failed to write to named pipe")?;
                pipe.flush()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                return Err(e).context("Failed to read from Unix socket");
            }
        }
        
        // Forward from named pipe to Unix socket
        match pipe.read(&mut pipe_buf) {
            Ok(0) => break,
            Ok(n) => {
                unix_stream.write_all(&pipe_buf[..n])
                    .context("Failed to write to Unix socket")?;
                unix_stream.flush()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                return Err(e).context("Failed to read from named pipe");
            }
        }
        
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    
    Ok(())
}
