//! 只运行一个桌面版：再次启动时把已经在运行的那个窗口调到前台，自己退出。
//!
//! 第一个启动的实例在本机回环地址上监听一个随机端口，把端口和一个随机口令写进数据目录的
//! `desktop.instance`。后来的实例读这个文件、连上去发口令，对方回 `ok` 就说明它还活着，于是退出。
//! 文件是上次留下的（程序崩溃、端口被别的程序占了）时连不上或者对不上口令，就自己当第一个。
//!
//! 只接受“显示窗口”这一个请求，口令只在本用户的数据目录里，别的程序能做的最多是把窗口调到前台。

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui_kit::App;

const FILE: &str = "desktop.instance";
const HELLO: &str = "dshr-show";

/// 已经确定自己是唯一的实例：之后把别的实例发来的请求交给界面。
pub struct SingleInstance {
    listener: TcpListener,
    token: String,
    /// 实例信息文件，正常退出（Drop）时删掉，下个实例不必连一个死端口白等超时。
    file: PathBuf,
}

/// 启动时调用。返回 `None` 表示已经有一个在运行，并且已经叫它把窗口调到前台了：这次启动应当直接退出。
/// 检查本身出了问题（数据目录写不了等）时照常启动，只是不再保证只有一个。
pub fn acquire(home: &Path) -> Option<Option<SingleInstance>> {
    let file = home.join(FILE);
    if notify_running(&file) {
        return None;
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok();
    let instance = listener.and_then(|listener| {
        let port = listener.local_addr().ok()?.port();
        let token = token();
        let _ = std::fs::create_dir_all(home);
        std::fs::write(&file, format!("{port} {token}")).ok()?;
        Some(SingleInstance { listener, token, file })
    });
    Some(instance)
}

/// 文件里记的实例还在就请它显示窗口，成功时返回 `true`。
fn notify_running(file: &PathBuf) -> bool {
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let mut parts = text.split_whitespace();
    let (Some(port), Some(token)) = (parts.next().and_then(|port| port.parse::<u16>().ok()), parts.next()) else {
        return false;
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    // 超时要短：这个文件正常退出时会被删掉，留在这里的多半是崩溃残留、端口已死。
    // 真有一个活实例时它在本机回环上，几十毫秒内一定接得上。
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(80)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    if writeln!(stream, "{HELLO} {token}").is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// 一个不容易猜到的口令：进程号、时间和标准库的随机哈希种子混在一起。
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    let first = hasher.finish();
    let mut second = std::collections::hash_map::RandomState::new().build_hasher();
    second.write_u64(first);
    format!("{first:016x}{:016x}", second.finish())
}

impl SingleInstance {
    /// 开始接收别的实例的请求；每收到一个就调用 `show`（在界面线程上）。
    pub fn serve(self, cx: &mut App, show: impl Fn(&mut App) + 'static) {
        let (sender, mut requests) = mpsc::unbounded::<()>();
        let Self { listener, token, file } = self;
        // 正常退出时删掉实例文件（GPUI 退出时 on_app_quit 触发）：下个实例不用连一个死端口白等超时。
        cx.on_app_quit(move |_| {
            let file = file.clone();
            async move {
                let _ = std::fs::remove_file(file);
            }
        })
        .detach();
        let spawned = std::thread::Builder::new().name("dshr-single-instance".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(1000)));
                let mut line = String::new();
                let Ok(reader) = stream.try_clone() else { continue };
                if BufReader::new(reader).read_line(&mut line).is_err() {
                    continue;
                }
                if line.trim() != format!("{HELLO} {token}") {
                    continue;
                }
                let mut stream = stream;
                let _ = writeln!(stream, "ok");
                if sender.unbounded_send(()).is_err() {
                    return;
                }
            }
        });
        if spawned.is_err() {
            return;
        }
        cx.spawn(async move |cx| {
            while requests.next().await.is_some() {
                cx.update(|cx| show(cx));
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_reaches_the_first() {
        let home = tempfile::tempdir().unwrap();
        // 没有文件、文件是坏的：都当第一个。
        assert!(!notify_running(&home.path().join(FILE)));
        std::fs::write(home.path().join(FILE), "not a port").unwrap();
        let first = acquire(home.path()).expect("应当是第一个").expect("应当能监听");

        // 第一个还没开始接收时，第二个连得上但等不到回复：照常启动。这里在另一条线程上手动回复。
        let token = first.token.clone();
        let listener = first.listener.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
            let mut stream = stream;
            if line.trim() == format!("{HELLO} {token}") {
                writeln!(stream, "ok").unwrap();
            }
        });
        assert!(acquire(home.path()).is_none(), "第二个应当找到第一个并退出");
        server.join().unwrap();

        // 口令对不上（文件被换过、端口被别的程序占了）：不算在运行。
        let text = std::fs::read_to_string(home.path().join(FILE)).unwrap();
        let port = text.split_whitespace().next().unwrap();
        std::fs::write(home.path().join(FILE), format!("{port} wrong")).unwrap();
        let listener = first.listener.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            let _ = BufReader::new(stream).read_line(&mut line);
        });
        assert!(!notify_running(&home.path().join(FILE)));
        server.join().unwrap();
    }
}
