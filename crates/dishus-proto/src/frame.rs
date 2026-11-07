//! Length-prefixed postcard framing with a hard size cap.

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds cap of {1}")]
    TooLarge(usize, usize),
    #[error("empty frame")]
    Empty,
    #[error("decode: {0}")]
    Decode(#[from] postcard::Error),
    #[error("invalid message: {0}")]
    Invalid(&'static str),
    #[error("stream closed")]
    Closed,
}

/// Something that can check its own fields after decoding.
pub trait Validate {
    fn validate(&self) -> Result<(), &'static str>;
}

/// Encode `msg` into a length-prefixed frame (no I/O). Fails if the encoding exceeds `cap`.
pub fn encode_frame<T: Serialize>(msg: &T, cap: usize) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > cap {
        return Err(FrameError::TooLarge(body.len(), cap));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode and validate one message from a frame body (without the length prefix).
pub fn decode_body<T: DeserializeOwned + Validate>(body: &[u8]) -> Result<T, FrameError> {
    if body.is_empty() {
        return Err(FrameError::Empty);
    }
    let (msg, rest): (T, &[u8]) = postcard::take_from_bytes(body)?;
    if !rest.is_empty() {
        return Err(FrameError::Invalid("trailing bytes"));
    }
    msg.validate().map_err(FrameError::Invalid)?;
    Ok(msg)
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T, cap: usize) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode_frame(msg, cap)?;
    w.write_all(&buf).await?;
    Ok(())
}

/// Read one frame. The length is checked against `cap` *before* any allocation.
/// Returns `Err(FrameError::Closed)` on clean EOF at a frame boundary.
pub async fn read_frame<R, T>(r: &mut R, cap: usize) -> Result<T, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned + Validate,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > cap {
        return Err(FrameError::TooLarge(len, cap));
    }
    if len == 0 {
        return Err(FrameError::Empty);
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    decode_body(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_CONTROL_FRAME;
    use crate::msg::{ClientMsg, MouseButton};

    #[tokio::test]
    async fn roundtrip() {
        let msgs = vec![
            ClientMsg::Ping { t: 42 },
            ClientMsg::Key {
                code: 30,
                down: true,
            },
            ClientMsg::MouseMove { dx: -5, dy: 300 },
            ClientMsg::MouseButton {
                button: MouseButton::Right,
                down: false,
            },
            ClientMsg::Scroll { dx: 0, dy: -120 },
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m, MAX_CONTROL_FRAME).await.unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            let got: ClientMsg = read_frame(&mut r, MAX_CONTROL_FRAME).await.unwrap();
            assert_eq!(&got, m);
        }
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut r, MAX_CONTROL_FRAME).await,
            Err(FrameError::Closed)
        ));
    }

    #[tokio::test]
    async fn oversize_length_rejected_without_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(u32::MAX).to_be_bytes());
        let mut r = &buf[..];
        assert!(matches!(
            read_frame::<_, ClientMsg>(&mut r, MAX_CONTROL_FRAME).await,
            Err(FrameError::TooLarge(_, _))
        ));
    }

    #[tokio::test]
    async fn garbage_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&3u32.to_be_bytes());
        buf.extend_from_slice(&[0xff, 0xff, 0xff]);
        let mut r = &buf[..];
        assert!(
            read_frame::<_, ClientMsg>(&mut r, MAX_CONTROL_FRAME)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn trailing_bytes_rejected() {
        let mut body = postcard::to_stdvec(&ClientMsg::Ping { t: 1 }).unwrap();
        body.push(0);
        assert!(matches!(
            decode_body::<ClientMsg>(&body),
            Err(FrameError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn invalid_field_rejected() {
        // keycode 0 is KEY_RESERVED and must not be accepted
        let body = postcard::to_stdvec(&ClientMsg::Key {
            code: 0,
            down: true,
        })
        .unwrap();
        assert!(matches!(
            decode_body::<ClientMsg>(&body),
            Err(FrameError::Invalid(_))
        ));
        let body = postcard::to_stdvec(&ClientMsg::Key {
            code: 116,
            down: true,
        })
        .unwrap();
        assert!(
            matches!(decode_body::<ClientMsg>(&body), Err(FrameError::Invalid(_))),
            "KEY_POWER"
        );
    }
}
