//! 本机端点（`handshake.md` §2、§4）：插件的监听端点与宿主的回调端点，Unix 域套接字、Windows 命名管道或回环 TCP。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::codegen::http::Uri;
use tonic::transport::server::Connected;

use super::HANDSHAKE_LINE_VERSION;
use crate::BoxFuture;

/// 端点的网络类型（`plugin.proto` 的 `Endpoint.network`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    /// Unix 域套接字 `unix`：放在本进程新建的私有目录里（目录 `0700`、套接字 `0600`）；只在 Unix 上可用。
    Unix,
    /// 回环 TCP `tcp`：只绑定 `127.0.0.1`，同机的其他用户连得上，隔离只靠会话令牌。
    Tcp,
    /// Windows 命名管道 `npipe`：拒绝远程客户端，用系统缺省的安全描述符（其他本机用户只能以只读打开、发不出请求），与回环 TCP
    /// 一样隔离靠会话令牌（`handshake.md` §4）；只在 Windows 上可用。
    NamedPipe,
}

impl Network {
    /// 协议里的名字：`unix`、`tcp` 或 `npipe`。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unix => "unix",
            Self::Tcp => "tcp",
            Self::NamedPipe => "npipe",
        }
    }

    /// 按协议里的名字解析，区分大小写；未知名字为 `None`。
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "unix" => Some(Self::Unix),
            "tcp" => Some(Self::Tcp),
            "npipe" => Some(Self::NamedPipe),
            _ => None,
        }
    }

    /// 本平台上插件监听的缺省网络：Unix 上是 Unix 域套接字，Windows 上是命名管道，其他平台是回环 TCP。
    pub const fn platform_default() -> Self {
        if cfg!(unix) {
            Self::Unix
        } else if cfg!(windows) {
            Self::NamedPipe
        } else {
            Self::Tcp
        }
    }

    /// 本平台上运行时能监听也能连接的网络，按回调时的偏好无关的固定顺序：Unix 上是 `unix` 与 `tcp`，Windows 上是 `npipe` 与 `tcp`，
    /// 其他平台只有 `tcp`。回调端点从 `XIHAN_PLUGIN_HOST_ADDRESS` 里按宿主的顺序取第一个 network 在这里的项（`handshake.md` §4）。
    pub const fn supported() -> &'static [Self] {
        if cfg!(unix) {
            &[Self::Unix, Self::Tcp]
        } else if cfg!(windows) {
            &[Self::NamedPipe, Self::Tcp]
        } else {
            &[Self::Tcp]
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 一个本机 gRPC 端点：网络类型与地址（套接字路径、`127.0.0.1:<port>` / `[::1]:<port>` 或管道名）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    network: Network,
    address: String,
}

impl Endpoint {
    /// 用网络类型与地址创建；不核对地址，来自外部的文本用 [`Endpoint::parse`]。
    pub fn new(network: Network, address: impl Into<String>) -> Self {
        Self { network, address: address.into() }
    }

    /// 解析一个 `<network> <address>`（握手行去掉版本之后的部分，`XIHAN_PLUGIN_HOST_ADDRESS` 的一项，`handshake.md` §2）：
    /// `network` 是 `unix`、`tcp` 或 `npipe`，与地址之间恰好一个空格；地址是一个或多个空白之外的字符（空白见 [`is_whitespace`]，
    /// 非 ASCII 字母与 `;` 都可以出现）；`tcp` 的地址只能是回环地址加端口，写法固定：`127.0.0.0/8` 的四段点分十进制（不带前导零）或
    /// `[::1]`，端口 `1`–`65535`（不带正负号与前导零）。不合法时为 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let (network, address) = text.split_once(' ')?;
        let network = Network::parse(network)?;
        if address.is_empty() || address.chars().any(is_whitespace) {
            return None;
        }
        if network == Network::Tcp && !is_loopback(address) {
            return None;
        }
        Some(Self::new(network, address))
    }

    /// 网络类型。
    pub fn network(&self) -> Network {
        self.network
    }

    /// 地址。
    pub fn address(&self) -> &str {
        &self.address
    }

    /// 握手行（`handshake.md` §2），不含结尾的 `\n`：`xihan-plugin/1 <network> <address>`。
    pub fn handshake_line(&self) -> String {
        format!("{HANDSHAKE_LINE_VERSION} {self}")
    }
}

impl fmt::Display for Endpoint {
    /// `<network> <address>`。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.network, self.address)
    }
}

/// 解析去掉行尾之后的握手行（`handshake.md` §2，向量 `plugin/handshake-lines.json`）：`xihan-plugin/1 <network> <address>`，
/// 前缀之后同 [`Endpoint::parse`]；不合法时为 `None`。
pub fn parse_handshake_line(line: &str) -> Option<Endpoint> {
    Endpoint::parse(line.strip_prefix(HANDSHAKE_LINE_VERSION)?.strip_prefix(' ')?)
}

/// 解析宿主回调端点的列表 `XIHAN_PLUGIN_HOST_ADDRESS`（`handshake.md` §4，向量 `plugin/host-addresses.json`）：一个或多个
/// `<network> <address>`，按宿主的偏好排序、以 `;` 分隔，每项同 [`Endpoint::parse`]、地址不含 `;`。任何一项不合法（含空项、
/// 首尾的 `;`）整个值就不合法，空串也不合法；不合法时为 `None`。
pub fn parse_host_addresses(value: &str) -> Option<Vec<Endpoint>> {
    // 按 ; 切开之后地址里自然没有 ;，空项在 Endpoint::parse 里不合法
    value.split(';').map(Endpoint::parse).collect()
}

/// 按 `handshake.md` §4 选回调端点：值合法时按顺序取第一个 network 在 `supported` 里的项；值不合法（变量缺失按空串传入）或没有
/// 能连的项时为 `None`，插件照常服务，回调以 `plugins.host.unavailable` 失败、不发请求。运行时用 [`Network::supported`]。
pub fn select_host_endpoint(value: &str, supported: &[Network]) -> Option<Endpoint> {
    parse_host_addresses(value)?.into_iter().find(|endpoint| supported.contains(&endpoint.network))
}

/// 握手行与回调端点所说的空白（`handshake.md` §2，即 RFC-0001「空白字符」，与 .NET 正则的 `\s` 相同）：U+0009–U+000D、U+0020、
/// U+0085、U+00A0、U+1680、U+2000–U+200A、U+2028、U+2029、U+202F、U+205F、U+3000；其余字符（含 U+200B 这样的零宽字符）都不是空白。
///
/// 集合按规范逐个写出，不用 [`char::is_whitespace`]：后者跟着 Unicode 的版本走，规范的集合是固定的。与 Error 的文案用同一个判断。
pub fn is_whitespace(c: char) -> bool {
    crate::grammar::is_whitespace(c)
}

/// `tcp` 端点的地址是不是回环地址加端口（`handshake.md` §2）：主机是 `127.0.0.0/8` 的四段点分十进制（每段 `0`–`255`、不带前导零）
/// 或 `[::1]`，端口是 `1`–`65535` 的十进制（不带正负号与前导零）。主机名（含 `localhost`）、`127.1`、`0177.0.0.1`、
/// `2130706433` 这些操作系统也接受的写法一律不算。
fn is_loopback(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    if decimal(port, 65_535).is_none_or(|port| port == 0) {
        return false;
    }
    if host == "[::1]" {
        return true;
    }
    let mut segments = host.split('.');
    segments.next() == Some("127") && (0..3).all(|_| segments.next().and_then(|segment| decimal(segment, 255)).is_some()) && segments.next().is_none()
}

/// 读不带正负号与前导零的十进制数（`0` 本身可以），只认 ASCII 数字，不超过 `limit`；不合写法时为 `None`。
fn decimal(text: &str, limit: u32) -> Option<u32> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    // 逐位累加并比较上限，过长的数字不会溢出
    text.bytes().try_fold(0u32, |value, byte| byte.is_ascii_digit().then(|| value * 10 + u32::from(byte - b'0')).filter(|value| *value <= limit))
}

/// 一条本机连接：服务端接受的，或回调宿主时建立的。
pub(crate) enum Io {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
}

/// 把一次读写转交给连接的具体类型；它们都是 `Unpin`，可以直接 `Pin::new`。
macro_rules! delegate {
    ($io:expr, $stream:ident => $call:expr) => {
        match $io.get_mut() {
            Io::Tcp($stream) => $call,
            #[cfg(unix)]
            Io::Unix($stream) => $call,
            #[cfg(windows)]
            Io::PipeServer($stream) => $call,
            #[cfg(windows)]
            Io::PipeClient($stream) => $call,
        }
    };
}

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_read(context, buffer))
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &[u8]) -> Poll<io::Result<usize>> {
        delegate!(self, stream => Pin::new(stream).poll_write(context, buffer))
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_flush(context))
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, stream => Pin::new(stream).poll_shutdown(context))
    }

    fn poll_write_vectored(self: Pin<&mut Self>, context: &mut Context<'_>, buffers: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        delegate!(self, stream => Pin::new(stream).poll_write_vectored(context, buffers))
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Io::Tcp(stream) => stream.is_write_vectored(),
            #[cfg(unix)]
            Io::Unix(stream) => stream.is_write_vectored(),
            #[cfg(windows)]
            Io::PipeServer(stream) => stream.is_write_vectored(),
            #[cfg(windows)]
            Io::PipeClient(stream) => stream.is_write_vectored(),
        }
    }
}

impl Connected for Io {
    // 本机连接没有要交给服务实现的对端信息：调用方的身份只看会话令牌
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

/// 正在监听的端点：接受的连接经通道交给 gRPC 服务端。
pub(crate) struct Listener {
    pub(crate) endpoint: Endpoint,
    pub(crate) incoming: mpsc::Receiver<io::Result<Io>>,
    pub(crate) accept: JoinHandle<()>,
    /// 为 Unix 域套接字新建的私有目录；停止后连同套接字删除。
    pub(crate) private_directory: Option<PathBuf>,
}

/// 接受失败（文件描述符用尽等）之后等一会儿再接受，不让失败的循环占满 CPU。
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// 按网络类型监听：Unix 域套接字放进新建的私有目录，TCP 只绑定 `127.0.0.1` 的随机端口，命名管道用随机名字、拒绝远程客户端。
/// 生成的地址都不含空白（`handshake.md` §2）。
pub(crate) async fn listen(network: Network) -> io::Result<Listener> {
    // 有界通道：服务端来不及取的连接在通道里排队，排满时接受循环等它，不在内存里堆积
    let (sender, incoming) = mpsc::channel(16);
    match network {
        Network::Tcp => {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let endpoint = Endpoint::new(Network::Tcp, listener.local_addr()?.to_string());
            let accept = tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            if sender.send(Ok(Io::Tcp(stream))).await.is_err() {
                                return;
                            }
                        }
                        Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                    }
                }
            });
            Ok(Listener { endpoint, incoming, accept, private_directory: None })
        }
        Network::Unix => listen_unix(sender, incoming).await,
        Network::NamedPipe => listen_pipe(sender, incoming),
    }
}

#[cfg(unix)]
async fn listen_unix(sender: mpsc::Sender<io::Result<Io>>, incoming: mpsc::Receiver<io::Result<Io>>) -> io::Result<Listener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    // 握手行的地址不能含空白（handshake.md §2），也要是 UTF-8 才能原样写出：临时目录的路径不满足时改用 /tmp
    let temp = std::env::temp_dir();
    let base = match temp.to_str() {
        Some(text) if !text.chars().any(is_whitespace) => temp,
        _ => PathBuf::from("/tmp"),
    };
    // 私有目录（handshake.md §4）：名字随机、权限 0700、只新建不复用；已经存在的可能是别人抢先建的、指向别处的链接，换个名字重来
    let mut attempts = 0;
    let directory = loop {
        let candidate = base.join(format!("xihan-plugin-{}", random_suffix()));
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => attempts += 1,
            Err(error) => return Err(error),
        }
    };
    let path = directory.join("plugin.sock");
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
    };
    // 目录已经只有本用户能进；套接字本身再收紧到 0600
    if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_dir_all(&directory);
        return Err(error);
    }
    let endpoint = Endpoint::new(Network::Unix, path.to_string_lossy().into_owned());
    let accept = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    if sender.send(Ok(Io::Unix(stream))).await.is_err() {
                        return;
                    }
                }
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            }
        }
    });
    Ok(Listener { endpoint, incoming, accept, private_directory: Some(directory) })
}

#[cfg(not(unix))]
async fn listen_unix(_: mpsc::Sender<io::Result<Io>>, _: mpsc::Receiver<io::Result<Io>>) -> io::Result<Listener> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Unix domain sockets are only available on Unix."))
}

#[cfg(windows)]
fn listen_pipe(sender: mpsc::Sender<io::Result<Io>>, incoming: mpsc::Receiver<io::Result<Io>>) -> io::Result<Listener> {
    use tokio::net::windows::named_pipe::ServerOptions;

    // first_pipe_instance：同名管道已经存在（可能是别人抢先建的）就失败，换个名字重来，不接到别人的管道上；
    // reject_remote_clients：拒绝远程客户端（handshake.md §4）。访问控制表要 unsafe 才设得了，本 crate 禁用 unsafe，
    // 所以用系统缺省的安全描述符：其他本机用户只能以只读打开、发不出请求，与回环 TCP 一样隔离靠会话令牌（§4 允许的做法）
    let mut attempts = 0;
    let (name, mut server) = loop {
        let name = format!(r"\\.\pipe\xihan-plugin-{}", random_suffix());
        match ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name) {
            Ok(server) => break (name, server),
            Err(_) if attempts < 8 => attempts += 1,
            Err(error) => return Err(error),
        }
    };
    let endpoint = Endpoint::new(Network::NamedPipe, name.clone());
    let accept = tokio::spawn(async move {
        loop {
            let connected = server.connect().await;
            // 先建好下一个实例再交出这一个：任何时刻都有实例在等连接，后来的客户端不会因为管道暂时不存在而失败；
            // 连接失败（客户端在接受之前就断开等）的实例不再复用，同样换成新的
            let next = loop {
                match ServerOptions::new().reject_remote_clients(true).create(&name) {
                    Ok(next) => break next,
                    Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
                }
            };
            let instance = std::mem::replace(&mut server, next);
            match connected {
                Ok(()) => {
                    if sender.send(Ok(Io::PipeServer(instance))).await.is_err() {
                        return;
                    }
                }
                Err(_) => tokio::time::sleep(ACCEPT_BACKOFF).await,
            }
        }
    });
    Ok(Listener { endpoint, incoming, accept, private_directory: None })
}

#[cfg(not(windows))]
fn listen_pipe(_: mpsc::Sender<io::Result<Io>>, _: mpsc::Receiver<io::Result<Io>>) -> io::Result<Listener> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Named pipes are only available on Windows."))
}

/// 连接一个本机端点（宿主回调服务）。
pub(crate) async fn connect(endpoint: &Endpoint) -> io::Result<Io> {
    match endpoint.network {
        Network::Tcp => TcpStream::connect(&endpoint.address).await.map(Io::Tcp),
        Network::Unix => connect_unix(&endpoint.address).await,
        Network::NamedPipe => connect_pipe(&endpoint.address).await,
    }
}

#[cfg(unix)]
async fn connect_unix(path: &str) -> io::Result<Io> {
    tokio::net::UnixStream::connect(path).await.map(Io::Unix)
}

#[cfg(not(unix))]
async fn connect_unix(_: &str) -> io::Result<Io> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Unix domain sockets are only available on Unix."))
}

#[cfg(windows)]
async fn connect_pipe(name: &str) -> io::Result<Io> {
    use tokio::net::windows::named_pipe::ClientOptions;

    /// 所有实例都在忙（`ERROR_PIPE_BUSY`）：服务端正在建下一个实例，稍等再连。
    const ERROR_PIPE_BUSY: i32 = 231;
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(name) {
            Ok(client) => return Ok(Io::PipeClient(client)),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 50 => attempts += 1,
            Err(error) => return Err(error),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(not(windows))]
async fn connect_pipe(_: &str) -> io::Result<Io> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Named pipes are only available on Windows."))
}

/// tonic 的连接器：每次建连接都按端点连一次。
#[derive(Clone)]
pub(crate) struct Connector {
    pub(crate) endpoint: Endpoint,
}

impl tonic::codegen::Service<Uri> for Connector {
    type Response = hyper_util::rt::TokioIo<Io>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<Self::Response>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let endpoint = self.endpoint.clone();
        Box::pin(async move { connect(&endpoint).await.map(hyper_util::rt::TokioIo::new) })
    }
}

/// 8 字节随机数的十六进制：私有目录与管道的名字。
///
/// # Panics
///
/// 操作系统的随机数源不可用时 panic。
fn random_suffix() -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap_or_else(|error| panic!("The system random number generator is unavailable: {error}."));
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_follow_the_grammar() {
        let endpoint = Endpoint::parse("unix /run/xihan/host-1f3a.sock").unwrap();
        assert_eq!((endpoint.network(), endpoint.address()), (Network::Unix, "/run/xihan/host-1f3a.sock"));
        assert_eq!(endpoint.to_string(), "unix /run/xihan/host-1f3a.sock");
        assert_eq!(endpoint.handshake_line(), "xihan-plugin/1 unix /run/xihan/host-1f3a.sock");
        for text in
            ["tcp 127.0.0.1:", "tcp 127.0.0.1:x", "tcp [::1]", "unix  /a", "unix /a b", "UNIX /a", "unix", "unix ", "unix /a\t", "tcp 127.0.0.1:99999999999"]
        {
            assert_eq!(Endpoint::parse(text), None, "{text:?}");
        }
        assert_eq!(Network::platform_default().as_str(), if cfg!(windows) { "npipe" } else { "unix" });
        assert!(Network::supported().contains(&Network::platform_default()) && Network::supported().ends_with(&[Network::Tcp]));
        // 规范的空白集合是固定的：U+200B、U+180E 不是空白
        assert!(is_whitespace('\u{3000}') && is_whitespace('\u{0B}') && !is_whitespace('\u{200B}') && !is_whitespace('\u{180E}'));
    }
}
