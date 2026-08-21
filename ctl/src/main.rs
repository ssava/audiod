use audcommon::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

fn usage() {
    eprintln!("audiod-ctl — control audiod server");
    eprintln!("Usage: audiod-ctl <command>");
    eprintln!("Commands:");
    eprintln!("  mute          Mute audio playback");
    eprintln!("  unmute        Unmute audio playback");
    eprintln!("  volume <pct>  Set volume in percent (0-100)");
    eprintln!("  status        Query server state");
    eprintln!("  -s <path>     Socket path (default: {})", SOCKET_PATH_STR);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut socket_path = SOCKET_PATH_STR;
    let mut cmd: Option<&str> = None;
    let mut vol_arg: Option<&str> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-s" => {
                i += 1;
                socket_path = args.get(i).map(|s| s.as_str()).unwrap_or(SOCKET_PATH_STR);
            }
            "-h" | "--help" => { usage(); return; }
            a if cmd.is_none() => { cmd = Some(a); }
            a if vol_arg.is_none() => { vol_arg = Some(a); }
            _ => { usage(); std::process::exit(1); }
        }
        i += 1;
    }

    let cmd = cmd.unwrap_or("status");

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => { eprintln!("connect to {}: {}", socket_path, e); std::process::exit(1); }
    };

    // Send control header: rate=0, channels=0, format=0
    stream.write_all(&[0u8; HDR_SZ]).expect("send header");

    match cmd {
        "mute" => {
            stream.write_all(&0u32.to_le_bytes()).unwrap();
            stream.write_all(&[MSG_MUTE]).unwrap();
            println!("muted");
        }
        "unmute" => {
            stream.write_all(&0u32.to_le_bytes()).unwrap();
            stream.write_all(&[MSG_UNMUTE]).unwrap();
            println!("unmuted");
        }
        "volume" => {
            let pct: f32 = vol_arg.and_then(|s| s.parse().ok()).unwrap_or(100.0);
            let pct = pct.clamp(0.0, 100.0);
            stream.write_all(&0u32.to_le_bytes()).unwrap();
            stream.write_all(&[MSG_VOLUME]).unwrap();
            stream.write_all(&pct.to_le_bytes()).unwrap();
            println!("volume set to {:.0}%", pct);
        }
        "status" => {
            stream.write_all(&0u32.to_le_bytes()).unwrap();
            stream.write_all(&[MSG_STATUS]).unwrap();
            let mut len_buf = [0u8; 4];
            if stream.read_exact(&mut len_buf).is_err() {
                eprintln!("no response from server");
                std::process::exit(1);
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];
            stream.read_exact(&mut data).unwrap();
            println!("{}", String::from_utf8_lossy(&data));
        }
        _ => { eprintln!("unknown command: {}", cmd); usage(); std::process::exit(1); }
    }
}
