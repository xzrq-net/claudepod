//! Nix daemon wire primitives (nix `serialise.cc`): u64 little-endian,
//! length-prefixed strings zero-padded to 8 bytes, and counted string lists.
//!
//! Read functions take explicit caps; pick them per direction.

use anyhow::{Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Cap on a single string from the guest. Everything a legitimate guest
/// sends is a store path (nix caps the name component at 211 bytes,
/// `path.hh` `MaxPathLen`), a protocol feature name, or a settings override
/// that the session discards — 64 KiB is orders of magnitude of slack.
pub const MAX_GUEST_STRING: u64 = 64 << 10;
/// Cap on a single string from the host daemon (error messages, log lines).
/// The host is trusted; this is only a sneeze guard.
pub const MAX_HOST_STRING: u64 = 16 << 20;
/// Cap on collection counts. The largest real collections are reference
/// closures, a few orders of magnitude below this.
pub const MAX_COUNT: u64 = 1 << 20;

pub async fn read_u64<R: AsyncRead + Unpin>(r: &mut R) -> Result<u64> {
    Ok(r.read_u64_le().await?)
}

/// Like `read_u64`, but a clean EOF before the first byte returns `None`.
pub async fn read_u64_or_eof<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<u64>> {
    let mut buf = [0u8; 8];
    let mut filled = 0;
    while filled < 8 {
        let n = r.read(&mut buf[filled..]).await?;
        if n == 0 {
            if filled == 0 {
                return Ok(None);
            }
            bail!("eof in the middle of a u64");
        }
        filled += n;
    }
    Ok(Some(u64::from_le_bytes(buf)))
}

pub async fn write_u64<W: AsyncWrite + Unpin>(w: &mut W, v: u64) -> Result<()> {
    w.write_u64_le(v).await?;
    Ok(())
}

pub async fn read_string<R: AsyncRead + Unpin>(r: &mut R, max_len: u64) -> Result<Vec<u8>> {
    let len = read_u64(r).await?;
    ensure!(
        len <= max_len,
        "string length {len} exceeds limit {max_len}"
    );
    let mut buf = vec![0; len as usize];
    r.read_exact(&mut buf).await?;
    if !len.is_multiple_of(8) {
        let mut pad = [0u8; 8];
        r.read_exact(&mut pad[..(8 - len % 8) as usize]).await?;
        ensure!(pad == [0u8; 8], "non-zero string padding");
    }
    Ok(buf)
}

pub async fn write_string<W: AsyncWrite + Unpin>(w: &mut W, s: &[u8]) -> Result<()> {
    write_u64(w, s.len() as u64).await?;
    w.write_all(s).await?;
    if !s.len().is_multiple_of(8) {
        w.write_all(&[0u8; 8][..8 - s.len() % 8]).await?;
    }
    Ok(())
}

pub async fn read_string_list<R: AsyncRead + Unpin>(
    r: &mut R,
    max_count: u64,
    max_len: u64,
) -> Result<Vec<Vec<u8>>> {
    let count = read_u64(r).await?;
    ensure!(
        count <= max_count,
        "string count {count} exceeds limit {max_count}"
    );
    let mut items = Vec::with_capacity(count as usize);
    for _ in 0..count {
        items.push(read_string(r, max_len).await?);
    }
    Ok(items)
}

pub async fn write_string_list<W, I>(w: &mut W, items: I) -> Result<()>
where
    W: AsyncWrite + Unpin,
    I: IntoIterator<Item: AsRef<[u8]>, IntoIter: ExactSizeIterator>,
{
    let items = items.into_iter();
    write_u64(w, items.len() as u64).await?;
    for item in items {
        write_string(w, item.as_ref()).await?;
    }
    Ok(())
}

pub async fn copy_u64<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
) -> Result<u64> {
    let v = read_u64(r).await?;
    write_u64(w, v).await?;
    Ok(v)
}

pub async fn copy_string<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
    max_len: u64,
) -> Result<()> {
    let s = read_string(r, max_len).await?;
    write_string(w, &s).await?;
    Ok(())
}

pub async fn copy_string_list<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut R,
    w: &mut W,
    max_len: u64,
) -> Result<()> {
    let count = read_u64(r).await?;
    ensure!(count <= MAX_COUNT, "string count {count} exceeds limit");
    write_u64(w, count).await?;
    for _ in 0..count {
        copy_string(r, w, max_len).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::testutil::{put_str, put_u64};

    #[tokio::test]
    async fn u64_fixture() {
        let mut out = Vec::new();
        write_u64(&mut out, 0x6e697863).await.unwrap();
        assert_eq!(out, [0x63, 0x78, 0x69, 0x6e, 0, 0, 0, 0]);
        assert_eq!(read_u64(&mut out.as_slice()).await.unwrap(), 0x6e697863);
    }

    #[tokio::test]
    async fn string_fixture() {
        let mut out = Vec::new();
        write_string(&mut out, b"hello").await.unwrap();
        let mut expected = vec![5, 0, 0, 0, 0, 0, 0, 0];
        expected.extend(b"hello");
        expected.extend([0, 0, 0]);
        assert_eq!(out, expected);
        assert_eq!(
            read_string(&mut out.as_slice(), MAX_GUEST_STRING)
                .await
                .unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn string_no_padding_when_multiple_of_8() {
        let mut out = Vec::new();
        write_string(&mut out, b"12345678").await.unwrap();
        assert_eq!(out.len(), 16);
        assert_eq!(
            read_string(&mut out.as_slice(), MAX_GUEST_STRING)
                .await
                .unwrap(),
            b"12345678"
        );
    }

    #[tokio::test]
    async fn empty_string() {
        let mut out = Vec::new();
        write_string(&mut out, b"").await.unwrap();
        assert_eq!(out, [0u8; 8]);
        assert_eq!(
            read_string(&mut out.as_slice(), MAX_GUEST_STRING)
                .await
                .unwrap(),
            b""
        );
    }

    #[tokio::test]
    async fn rejects_nonzero_padding() {
        let mut buf = Vec::new();
        put_u64(&mut buf, 5);
        buf.extend(b"hello");
        buf.extend([0, 0, 1]);
        let err = read_string(&mut buf.as_slice(), MAX_GUEST_STRING)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("padding"), "{err}");
    }

    #[tokio::test]
    async fn rejects_oversized_string() {
        let mut buf = Vec::new();
        put_u64(&mut buf, MAX_GUEST_STRING + 1);
        let err = read_string(&mut buf.as_slice(), MAX_GUEST_STRING)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
    }

    #[tokio::test]
    async fn u64_or_eof() {
        assert_eq!(read_u64_or_eof(&mut [].as_slice()).await.unwrap(), None);
        let mut buf = Vec::new();
        put_u64(&mut buf, 7);
        assert_eq!(read_u64_or_eof(&mut buf.as_slice()).await.unwrap(), Some(7));
        let err = read_u64_or_eof(&mut buf[..4].as_ref()).await.unwrap_err();
        assert!(err.to_string().contains("middle"), "{err}");
    }

    #[tokio::test]
    async fn string_list_roundtrip_and_copy() {
        let mut buf = Vec::new();
        write_string_list(&mut buf, [b"one".as_slice(), b"twotwo".as_slice()])
            .await
            .unwrap();
        let items = read_string_list(&mut buf.as_slice(), MAX_COUNT, MAX_GUEST_STRING)
            .await
            .unwrap();
        assert_eq!(items, vec![b"one".to_vec(), b"twotwo".to_vec()]);

        let mut copied = Vec::new();
        copy_string_list(&mut buf.as_slice(), &mut copied, MAX_GUEST_STRING)
            .await
            .unwrap();
        assert_eq!(copied, buf);
    }

    #[tokio::test]
    async fn rejects_oversized_list() {
        let mut buf = Vec::new();
        put_u64(&mut buf, 3);
        let err = read_string_list(&mut buf.as_slice(), 2, MAX_GUEST_STRING)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds limit"), "{err}");
    }

    #[tokio::test]
    async fn copy_is_byte_identical() {
        let mut buf = Vec::new();
        put_u64(&mut buf, 42);
        put_str(&mut buf, b"abc");
        let mut input = buf.as_slice();
        let mut out = Vec::new();
        assert_eq!(copy_u64(&mut input, &mut out).await.unwrap(), 42);
        copy_string(&mut input, &mut out, MAX_GUEST_STRING)
            .await
            .unwrap();
        assert_eq!(out, buf);
    }
}
