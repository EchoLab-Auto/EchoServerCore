//! 单向计数管道：`copy_bidirectional` 的替代品。
//!
//! 为什么不用 `tokio::io::copy_bidirectional`：它只在**整条连接结束时**才把
//! 两个方向的字节数返回。面板要显示「现在走到多少字节」，需要的是过程中就能
//! 读到的累计值，所以这里自己写一个 16 KiB 的搬运循环，每读一段就加到
//! `AtomicU64` 上。

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 一条连接的搬运缓冲大小。16 KiB 在吞吐与内存之间比较平衡
/// （8 KiB 时小包多，64 KiB 时 128 并发的常驻内存会明显上去）。
const BUF: usize = 16 * 1024;

/// 把 `r` 里的字节搬到 `w`，并把每段长度累加到 `counter`。
///
/// 返回这一方向搬运的总字节数；任一侧出错即返回该错误。
pub async fn pipe<R, W>(mut r: R, mut w: W, counter: &AtomicU64) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF];
    let mut total: u64 = 0;
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        w.write_all(&buf[..n]).await?;
        total += n as u64;
        // 逐段累加：面板随时读都是「到目前为止」的准确值。
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }
    w.flush().await?;
    // 让对端及时看到 FIN，否则对向管道会一直等不到 EOF。
    let _ = w.shutdown().await;
    Ok(total)
}

/// 双向搬运，两个方向共享同一个连接。返回 `(上行, 下行)`。
pub async fn duplex<A, B>(
    a: A,
    b: B,
    up: &AtomicU64,
    down: &AtomicU64,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    B: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut a_r, mut a_w) = tokio::io::split(a);
    let (mut b_r, mut b_w) = tokio::io::split(b);
    let a_to_b = pipe(&mut a_r, &mut b_w, up);
    let b_to_a = pipe(&mut b_r, &mut a_w, down);
    tokio::try_join!(a_to_b, b_to_a)
}
