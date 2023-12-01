use std::{
    cmp::min,
    fmt::Debug,
    io::{self, ErrorKind, Read, Write},
    iter::Iterator,
    marker::PhantomData,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream as StdUnixStream,
    },
    str::from_utf8,
    time::Duration,
};

use mio::{event::Source, net::UnixStream as MioUnixStream};
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json;

use crate::{buffer::growable::Buffer, ready::Ready};

#[derive(thiserror::Error, Debug)]
pub enum ChannelError {
    #[error("io read error")]
    Read(std::io::Error),
    #[error("no byte written on the channel")]
    NoByteWritten,
    #[error("no byte left to read on the channel")]
    NoByteToRead,
    #[error("message too large for the capacity of the back fuffer ({0}. Consider increasing the back buffer size")]
    MessageTooLarge(usize),
    #[error("channel could not write on the back buffer")]
    Write(std::io::Error),
    #[error("channel buffer is full, cannot grow more")]
    BufferFull,
    #[error("Timeout is reached: {0:?}")]
    TimeoutReached(Duration),
    #[error("Could not read anything on the channel")]
    NothingRead,
    #[error("invalid char set in command message, ignoring: {0}")]
    InvalidCharSet(String),
    #[error("Error deserializing message")]
    Serde(serde_json::error::Error),
    #[error("Could not change the blocking status ef the unix stream with file descriptor {fd}: {error}")]
    BlockingStatus { fd: i32, error: String },
    #[error("Connection error: {0:?}")]
    Connection(Option<std::io::Error>),
}

/// A wrapper around unix socket using the mio crate.
/// Used in pairs to communicate, in a blocking or non-blocking way.
pub struct Channel<Tx, Rx> {
    pub sock: MioUnixStream,
    front_buf: Buffer,
    pub back_buf: Buffer,
    max_buffer_size: usize,
    pub readiness: Ready,
    pub interest: Ready,
    blocking: bool,
    phantom_tx: PhantomData<Tx>,
    phantom_rx: PhantomData<Rx>,
}

impl<Tx: Debug + Serialize, Rx: Debug + DeserializeOwned> Channel<Tx, Rx> {
    /// Creates a nonblocking channel on a given socket path
    pub fn from_path(
        path: &str,
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> Result<Channel<Tx, Rx>, ChannelError> {
        let unix_stream = MioUnixStream::connect(path)
            .map_err(|io_error| ChannelError::Connection(Some(io_error)))?;
        Ok(Channel::new(unix_stream, buffer_size, max_buffer_size))
    }

    /// Creates a nonblocking channel, using a unix stream
    pub fn new(sock: MioUnixStream, buffer_size: usize, max_buffer_size: usize) -> Channel<Tx, Rx> {
        Channel {
            sock,
            front_buf: Buffer::with_capacity(buffer_size),
            back_buf: Buffer::with_capacity(buffer_size),
            max_buffer_size,
            readiness: Ready::EMPTY,
            interest: Ready::READABLE,
            blocking: false,
            phantom_tx: PhantomData,
            phantom_rx: PhantomData,
        }
    }

    pub fn into<Tx2: Debug + Serialize, Rx2: Debug + DeserializeOwned>(self) -> Channel<Tx2, Rx2> {
        Channel {
            sock: self.sock,
            front_buf: self.front_buf,
            back_buf: self.back_buf,
            max_buffer_size: self.max_buffer_size,
            readiness: self.readiness,
            interest: self.interest,
            blocking: self.blocking,
            phantom_tx: PhantomData,
            phantom_rx: PhantomData,
        }
    }

    // Since MioUnixStream does not have a set_nonblocking method, we have to use the standard library.
    // We get the file descriptor of the MioUnixStream socket, create a standard library UnixStream,
    // set it to nonblocking, let go of the file descriptor
    fn set_nonblocking(&mut self, nonblocking: bool) -> Result<(), ChannelError> {
        unsafe {
            let fd = self.sock.as_raw_fd();
            let stream = StdUnixStream::from_raw_fd(fd);
            stream
                .set_nonblocking(nonblocking)
                .map_err(|error| ChannelError::BlockingStatus {
                    fd,
                    error: error.to_string(),
                })?;
            let _fd = stream.into_raw_fd();
        }
        self.blocking = !nonblocking;
        Ok(())
    }

    /// set the channel to be blocking
    pub fn blocking(&mut self) -> Result<(), ChannelError> {
        self.set_nonblocking(false)
    }

    /// set the channel to be nonblocking
    pub fn nonblocking(&mut self) -> Result<(), ChannelError> {
        self.set_nonblocking(true)
    }

    pub fn is_blocking(&self) -> bool {
        self.blocking
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    pub fn handle_events(&mut self, events: Ready) {
        self.readiness |= events;
    }

    pub fn readiness(&self) -> Ready {
        self.readiness & self.interest
    }

    /// Checks wether we want and can read or write, and calls the appropriate handler.
    pub fn run(&mut self) -> Result<(), ChannelError> {
        let interest = self.interest & self.readiness;

        if interest.is_readable() {
            let _ = self.readable()?;
        }

        if interest.is_writable() {
            let _ = self.writable()?;
        }
        Ok(())
    }

    /// Handles readability by filling the front buffer with the socket data.
    pub fn readable(&mut self) -> Result<usize, ChannelError> {
        // 判断当前 channel 是否是可读的, 如果是非可读的, 则返回错误
        if !(self.interest & self.readiness).is_readable() {
            return Err(ChannelError::Connection(None));
        }

        // 字节读取数
        let mut count = 0usize;
        loop {
            // 判断 front buf  是否还有空间缓存数据
            let size = self.front_buf.available_space();
            // 如果 size 为 0 则代表 buf 已经满了, 则把 Ready::READABLE 从 interest 中移除
            if size == 0 {
                self.interest.remove(Ready::READABLE);
                break;
            }

            // 从 socket 中读取数据到 front buf 中
            match self.sock.read(self.front_buf.space()) {
                // 如果读取到的数据为 0, 代表 socket 已经关闭或者 huang up, 则把 interest 设置为 Ready::EMPTY
                // readiness 移除 Ready::READABLE, 并且把 Ready::HUP 添加到 readiness 中
                Ok(0) => {
                    self.interest = Ready::EMPTY;
                    self.readiness.remove(Ready::READABLE);
                    self.readiness.insert(Ready::HUP);
                    return Err(ChannelError::NoByteToRead);
                }
                // 读取 socket 发生错误, 如果错误类型为 WouldBlock, 则把 Ready::READABLE 从 readiness 中移除 
                Err(read_error) => match read_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::READABLE);
                        break;
                    }
                    _ => {
                        // 其他错误类型直接返回错误
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::EMPTY;
                        return Err(ChannelError::Read(read_error));
                    }
                },
                Ok(bytes_read) => {
                    // 读取到数据, 则把读取到的数据大小加到 count 中
                    count += bytes_read;
                    self.front_buf.fill(bytes_read);
                }
            };
        }

        Ok(count)
    }

    /// Handles writability by writing the content of the back buffer onto the socket
    pub fn writable(&mut self) -> Result<usize, ChannelError> {
        // 判断当前 channel 是否是可写的, 如果是非可写的, 则返回错误
        if !(self.interest & self.readiness).is_writable() {
            return Err(ChannelError::Connection(None));
        }

        // 字节写入数
        let mut count = 0usize;
        loop {
            // 判断 back buf 是否拥有可写的数据
            let size = self.back_buf.available_data();
            if size == 0 {
                // 如果 size 等于 0 则代表 buffer 中无可写的数据了
                // 从 interest 中移除 Ready::WRITABLE 代表当前 buffer 不可写
                self.interest.remove(Ready::WRITABLE);
                break;
            }

            // 把 back buf 中的数据写入到 socket 中
            match self.sock.write(self.back_buf.data()) {
                // 写入数据为 0, 代表 socket 已经关闭或者 huang up, 则把 interest 设置为 Ready::EMPTY
                Ok(0) => {
                    self.interest = Ready::EMPTY;
                    self.readiness.insert(Ready::HUP);
                    return Err(ChannelError::NoByteWritten);
                }
                // 写入数据不为 0, 增加写入数据的大小到 count 中, 并且从 back buf 中移除写入的数据
                Ok(bytes_written) => {
                    count += bytes_written;
                    self.back_buf.consume(bytes_written);
                }
                // 如果发生错误, 则判断是否是 WouldBlock ,如果是 WouldBlock 则从 readiness 移除 Ready::WRITABLE
                // 否则把 interest 设置为 Ready::EMPTY, readiness 设置为 Ready::EMPTY, 并且返回错误
                Err(write_error) => match write_error.kind() {
                    ErrorKind::WouldBlock => {
                        self.readiness.remove(Ready::WRITABLE);
                        break;
                    }
                    _ => {
                        self.interest = Ready::EMPTY;
                        self.readiness = Ready::EMPTY;
                        return Err(ChannelError::Read(write_error));
                    }
                },
            }
        }

        Ok(count)
    }

    /// Depending on the blocking status:
    ///
    /// Blocking: waits for the front buffer to be filled, and parses a message from it
    ///
    /// Nonblocking: parses a message from the front buffer, without waiting.
    /// Prefer using `channel.readable()` before
    pub fn read_message(&mut self) -> Result<Rx, ChannelError> {
        if self.blocking {
            self.read_message_blocking()
        } else {
            self.read_message_nonblocking()
        }
    }

    /// Parses a message from the front buffer, without waiting
    fn read_message_nonblocking(&mut self) -> Result<Rx, ChannelError> {
        match self.front_buf.data().iter().position(|&x| x == 0) {
            Some(position) => self.read_and_parse_from_front_buffer(position),
            None => {
                if self.front_buf.available_space() == 0 {
                    if self.front_buf.capacity() == self.max_buffer_size {
                        error!("command buffer full, cannot grow more, ignoring");
                    } else {
                        let new_size = min(self.front_buf.capacity() + 5000, self.max_buffer_size);
                        self.front_buf.grow(new_size);
                    }
                }

                self.interest.insert(Ready::READABLE);
                Err(ChannelError::NothingRead)
            }
        }
    }

    fn read_message_blocking(&mut self) -> Result<Rx, ChannelError> {
        self.read_message_blocking_timeout(None)
    }

    /// Waits for the front buffer to be filled, and parses a message from it.
    /// 阻塞读取消息, 并支持读取超时时间
    pub fn read_message_blocking_timeout(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Rx, ChannelError> {
        let now = std::time::Instant::now();

        loop {
            if let Some(timeout) = timeout {
                // 判断是否已经超时, 如果已经超时则直接抛出异常
                if now.elapsed() >= timeout {
                    return Err(ChannelError::TimeoutReached(timeout));
                }
            }

            // 获取 buffer 中的数据切片[position..end], 并获取 0 的位置, 相当于是寻找结束符
            match self.front_buf.data().iter().position(|&x| x == 0) {
                Some(position) => return self.read_and_parse_from_front_buffer(position),
                None => {
                    // 判断 buffer 是否已经满了
                    if self.front_buf.available_space() == 0 {
                        // 如果 buffer 已经满了, 则判断 buffer 的容量是否已经达到 channel 中指定的最大容量, 如果达到最大容量则直接返回错误
                        if self.front_buf.capacity() == self.max_buffer_size {
                            return Err(ChannelError::BufferFull);
                        }
                        // 如果没达到 channel 中指定的 buffer 的最大容量, 则进行扩容, 默认一次性扩容 5000
                        let new_size = min(self.front_buf.capacity() + 5000, self.max_buffer_size);
                        self.front_buf.grow(new_size);
                    }

                    // 扩容之后继续从 socket 中读取数据到  buffer 中
                    match self
                        .sock
                        .read(self.front_buf.space())
                        .map_err(ChannelError::Read)?
                    {
                        0 => return Err(ChannelError::NoByteToRead),
                        bytes_read => self.front_buf.fill(bytes_read),
                    };
                }
            }
        }
    }

    fn read_and_parse_from_front_buffer(&mut self, position: usize) -> Result<Rx, ChannelError> {
        // 从 front buf 中读取数据, 并转换为 utf8 字符串
        let utf8_str = from_utf8(&self.front_buf.data()[..position])
            .map_err(|from_error| ChannelError::InvalidCharSet(from_error.to_string()))?;
        // 字符串转换为 json
        let json_parsed = serde_json::from_str(utf8_str).map_err(ChannelError::Serde)?;
        // 读取完数据之后更新 position , 并判断 position 位置是否超过 buffer 的 2分之1 如果超过则作 shift 操作,
        // shit 操作就是把 buffer 中剩余的数据全部移动到最左侧, 并更新 position 为 0 , end 为移动字节数的数量
        self.front_buf.consume(position + 1);
        Ok(json_parsed)
    }

    /// Checks whether the channel is blocking or nonblocking, writes the message.
    ///
    /// If the channel is nonblocking, you have to flush using `channel.run()` afterwards
    pub fn write_message(&mut self, message: &Tx) -> Result<(), ChannelError> {
        if self.blocking {
            self.write_message_blocking(message)
        } else {
            self.write_message_nonblocking(message)
        }
    }

    /// Writes the message in the buffer, but NOT on the socket.
    /// you have to call channel.run() afterwards
    /// 非阻塞写入数据, 把数据写入到 buffer 中, 并把 Ready::WRITABLE 添加到 interest 中, 
    /// 并不直接写入到 socket, 所以后续需要调用 channel.run() flush buffer
    fn write_message_nonblocking(&mut self, message: &Tx) -> Result<(), ChannelError> {
        // 把需要写入的数据转换为 byte 数组
        let message = match serde_json::to_string(message) {
            Ok(string) => string.into_bytes(),
            Err(_) => Vec::new(),
        };

        // len + 1 这个 1表示后面新增一个结束符
        let message_len = message.len() + 1;
        // 如果 消息长度大于 buffer 的可用空间, 则把 buffer 中的数据向左移动
        if message_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        // 如果移动之后依然大于
        if message_len > self.back_buf.available_space() {
            // 如果消息长度填充了之后,剩余的空间加上 buffer 的容量大于 channel 中指定的最大容量, 则直接返回错误
            if message_len - self.back_buf.available_space() + self.back_buf.capacity()
                > self.max_buffer_size
            {
                return Err(ChannelError::MessageTooLarge(self.back_buf.capacity()));
            }

            // 计算出能够容纳下消息的 buffer 的容量, 并进行扩容
            let new_length =
                message_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_length);
        }

        // 写入数据到 buffer 中
        self.back_buf.write(&message).map_err(ChannelError::Write)?;

        // 写入结束符
        self.back_buf
            .write(&b"\0"[..])
            .map_err(ChannelError::Write)?;
        // 把 Ready::WRITABLE 添加到 interest 中
        self.interest.insert(Ready::WRITABLE);

        Ok(())
    }

    /// fills the back buffer with data AND writes on the socket
    /// 阻塞写入数据, 把数据写入到 buffer 中.
    fn write_message_blocking(&mut self, message: &Tx) -> Result<(), ChannelError> {
        // 把 message 转换成 byte 数组
        let message = match serde_json::to_string(message) {
            Ok(string) => string.into_bytes(),
            Err(_) => Vec::new(),
        };

        // 计算 message 的长度 并 + 1, 这个 1 表示后面新增一个结束符
        let msg_len = &message.len() + 1;
        // 判断 message 的长度是否大于 buffer 的可用空间, 如果大于则把 buffer 中的数据向左移动
        if msg_len > self.back_buf.available_space() {
            self.back_buf.shift();
        }

        // 判断 message 的长度是否大于 buffer 的可用空间, 如果大于则进行扩容
        if msg_len > self.back_buf.available_space() {
            // 如果消息长度填充了之后,剩余的空间加上 buffer 的容量大于 channel 中指定的最大容量, 则直接返回错误
            if msg_len - self.back_buf.available_space() + self.back_buf.capacity()
                > self.max_buffer_size
            {
                return Err(ChannelError::MessageTooLarge(self.back_buf.capacity()));
            }
            // 计算出能够容纳下消息的 buffer 的容量, 并进行扩容
            let new_len = msg_len - self.back_buf.available_space() + self.back_buf.capacity();
            self.back_buf.grow(new_len);
        }

        // 把 message 写入到 buffer 中
        self.back_buf.write(&message).map_err(ChannelError::Write)?;

        // 把结束符写入到 buffer 中
        self.back_buf
            .write(&b"\0"[..])
            .map_err(ChannelError::Write)?;

        loop {
            // 判断 back buf 中是否还有可写的数据
            let size = self.back_buf.available_data();
            if size == 0 {
                break;
            }

            // 把 back buf 中的数据写入到 socket 中
            match self.sock.write(self.back_buf.data()) {
                Ok(0) => return Err(ChannelError::NoByteWritten),
                Ok(bytes_written) => {
                    // 调整 position index
                    self.back_buf.consume(bytes_written);
                }
                Err(_) => return Ok(()), // are we sure?
            }
        }
        Ok(())
    }
}

type ChannelResult<Tx, Rx> = Result<(Channel<Tx, Rx>, Channel<Rx, Tx>), ChannelError>;

impl<Tx: Debug + DeserializeOwned + Serialize, Rx: Debug + DeserializeOwned + Serialize>
    Channel<Tx, Rx>
{
    /// creates a channel pair: `(blocking_channel, nonblocking_channel)`
    pub fn generate(buffer_size: usize, max_buffer_size: usize) -> ChannelResult<Tx, Rx> {
        // 创建一个unix socket pair, 由 mio 提供. 这个类似于管道 channel
        // 这里的 command stream 是 channel 里面的 tx, proxy stream 是 channel 里面的 rx
        let (command, proxy) = MioUnixStream::pair().map_err(ChannelError::Read)?;
        // 使用 proxy unix stream 创建一个 非阻塞 channel
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        // 使用 command unix stream 创建一个 非阻塞 channel
        let mut command_channel = Channel::new(command, buffer_size, max_buffer_size);
        // 设置 command_channel 为阻塞模式
        command_channel.blocking()?;
        // 返回 command_channel 和 proxy_channel
        Ok((command_channel, proxy_channel))
    }

    /// creates a pair of nonblocking channels
    pub fn generate_nonblocking(
        buffer_size: usize,
        max_buffer_size: usize,
    ) -> ChannelResult<Tx, Rx> {
        let (command, proxy) = MioUnixStream::pair().map_err(ChannelError::Read)?;
        let proxy_channel = Channel::new(proxy, buffer_size, max_buffer_size);
        let command_channel = Channel::new(command, buffer_size, max_buffer_size);
        Ok((command_channel, proxy_channel))
    }
}

impl<Tx: Debug + Serialize, Rx: Debug + DeserializeOwned> Iterator for Channel<Tx, Rx> {
    type Item = Rx;
    fn next(&mut self) -> Option<Self::Item> {
        self.read_message().ok()
    }
}

/**
 * 实现 Source trait, 用于注册到 mio 的 event loop 中
 */
use mio::{Interest, Registry, Token};
impl<Tx, Rx> Source for Channel<Tx, Rx> {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.sock.register(registry, token, interests)
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        self.sock.reregister(registry, token, interests)
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        self.sock.deregister(registry)
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Serializable(u32);

    #[test]
    fn unblock_a_channel() {
        let (mut blocking, _nonblocking): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("could not generate blocking channels");
        assert!(blocking.nonblocking().is_ok())
    }

    #[test]
    fn generate_blocking_and_nonblocking_channels() {
        let (blocking_channel, nonblocking_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("could not generatie blocking channels");

        assert!(blocking_channel.is_blocking());
        assert!(!nonblocking_channel.is_blocking());

        let (nonblocking_channel_1, nonblocking_channel_2): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate_nonblocking(1000, 10000)
            .expect("could not generatie nonblocking channels");

        assert!(!nonblocking_channel_1.is_blocking());
        assert!(!nonblocking_channel_2.is_blocking());
    }

    #[test]
    fn write_and_read_message_blocking() {
        let (mut blocking_channel, mut nonblocking_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");

        let message_to_send = Serializable(42);

        nonblocking_channel
            .blocking()
            .expect("Could not block channel");
        nonblocking_channel
            .write_message(&message_to_send)
            .expect("Could not write message on channel");

        println!("we wrote a message!");

        println!("reading message..");
        // blocking_channel.readable();
        let message = blocking_channel
            .read_message()
            .expect("Could not read message on channel");
        println!("read message!");

        assert_eq!(message, Serializable(42));
    }

    #[test]
    fn read_message_blocking_with_timeout_fails() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking().expect("Could not block channel");

        println!("reading message in a detached thread, with a timeout of 100 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message =
                reading_channel.read_message_blocking_timeout(Some(Duration::from_millis(100)));
            println!("read message!");
            message
        });

        println!("Waiting 200 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(200));

        writing_channel
            .write_message(&Serializable(200))
            .expect("Could not write message on channel");
        println!("we wrote a message that should arrive too late!");

        let arrived_too_late = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert!(arrived_too_late.is_err());
    }

    #[test]
    fn read_message_blocking_with_timeout_succeeds() {
        let (mut reading_channel, mut writing_channel): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        writing_channel.blocking().expect("Could not block channel");

        println!("reading message in a detached thread, with a timeout of 200 milliseconds...");
        let awaiting_with_timeout = thread::spawn(move || {
            let message = reading_channel
                .read_message_blocking_timeout(Some(Duration::from_millis(200)))
                .expect("Could not read message with timeout on blocking channel");
            println!("read message!");
            message
        });

        println!("Waiting 100 milliseconds…");
        thread::sleep(std::time::Duration::from_millis(100));

        writing_channel
            .write_message(&Serializable(100))
            .expect("Could not write message on channel");
        println!("we wrote a message that should arrive on time!");

        let arrived_on_time = awaiting_with_timeout
            .join()
            .expect("error with receiving message from awaiting thread");

        assert_eq!(arrived_on_time, Serializable(100));
    }

    #[test]
    fn exhaustive_use_of_nonblocking_channels() {
        // - two nonblocking channels A and B, identical
        let (mut channel_a, mut channel_b): (
            Channel<Serializable, Serializable>,
            Channel<Serializable, Serializable>,
        ) = Channel::generate(1000, 10000).expect("Could not create nonblocking channel");
        channel_a.nonblocking().expect("Could not block channel");

        // write on A
        channel_a
            .write_message(&Serializable(1))
            .expect("Could not write message on channel");

        // set B as readable, normally mio tells when to, by giving events
        channel_b.handle_events(Ready::READABLE);

        // read on B
        let should_err = channel_b.read_message();
        assert!(should_err.is_err());

        // write another message on A
        channel_a
            .write_message(&Serializable(2))
            .expect("Could not write message on channel");

        // insert a handle_events Ready::writable on A
        channel_a.handle_events(Ready::WRITABLE);

        // flush A with run()
        channel_a.run().expect("Failed to run the channel");

        // maybe a thread sleep
        // thread::sleep(std::time::Duration::from_millis(100));

        // receive with B using run()
        channel_b.run().expect("Failed to run the channel");

        // use read_message() twice on B, check them
        let message_1 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_1, Serializable(1));

        let message_2 = channel_b
            .read_message()
            .expect("Could not read message on channel");
        assert_eq!(message_2, Serializable(2));
    }
}
