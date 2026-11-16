//! A video kept in a `.uxflight` played from where it is (ADR-0062): the
//! file stores it without compression, so it is one run of the file's
//! bytes, served with Range requests as `ServeFile` serves a whole file, so
//! a `<video>` seeks in it.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// How much of the file is read at a time.
const CHUNK: usize = 256 * 1024;

/// The bytes a `Range` header asks for of `len`, first and last, or None
/// for all of them. One range only, as a `<video>` asks.
///
/// # Errors
/// It asks for none of them: a 416.
fn wanted(range: Option<&str>, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(range) = range else {
        return Ok(None);
    };
    let Some(spec) = range.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    // Several ranges: the whole, as ServeFile answers them.
    if spec.contains(',') {
        return Ok(None);
    }
    let (first, last) = spec.split_once('-').ok_or(())?;
    let (first, last) = match (first.trim(), last.trim()) {
        ("", "") => return Err(()),
        ("", suffix) => {
            let n: u64 = suffix.parse().map_err(|_| ())?;
            if n == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len.checked_sub(1).ok_or(())?)
        }
        (first, "") => (
            first.parse().map_err(|_| ())?,
            len.checked_sub(1).ok_or(())?,
        ),
        (first, last) => {
            let first: u64 = first.parse().map_err(|_| ())?;
            let last: u64 = last.parse().map_err(|_| ())?;
            (first, last.min(len.saturating_sub(1)))
        }
    };
    if first > last || first >= len {
        return Err(());
    }
    Ok(Some((first, last)))
}

/// What a request asks for: its `Range` header, and whether it is a `HEAD`.
#[must_use]
pub fn asked(request: &Request) -> (Option<String>, bool) {
    (
        request
            .headers()
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        request.method() == Method::HEAD,
    )
}

/// Answers a request with `len` bytes of `path` from `offset`, as
/// `content_type`: all of them, or the `range` asked for with a 206.
pub async fn serve(
    path: std::path::PathBuf,
    offset: u64,
    len: u64,
    content_type: &'static str,
    (range, head): (Option<String>, bool),
) -> Response {
    let Ok(wanted) = wanted(range.as_deref(), len) else {
        return (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response();
    };
    let (status, first, count) = match wanted {
        Some((first, last)) => (StatusCode::PARTIAL_CONTENT, first, last - first + 1),
        None => (StatusCode::OK, 0, len),
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) => return (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset + first)).await {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let body = if head {
        Body::empty()
    } else {
        let stream =
            futures::stream::unfold((file.take(count), false), |(mut from, ended)| async move {
                if ended {
                    return None;
                }
                let mut buffer = vec![0u8; CHUNK];
                match from.read(&mut buffer).await {
                    Ok(0) => None,
                    Ok(n) => {
                        buffer.truncate(n);
                        Some((Ok::<_, std::io::Error>(buffer), (from, false)))
                    }
                    Err(e) => Some((Err(e), (from, true))),
                }
            });
        Body::from_stream(stream)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(count));
    if status == StatusCode::PARTIAL_CONTENT
        && let Ok(value) =
            HeaderValue::from_str(&format!("bytes {first}-{}/{len}", first + count - 1))
    {
        headers.insert(header::CONTENT_RANGE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_read_as_a_video_element_asks() {
        assert_eq!(wanted(None, 100), Ok(None));
        assert_eq!(wanted(Some("bytes=0-"), 100), Ok(Some((0, 99))));
        assert_eq!(wanted(Some("bytes=10-19"), 100), Ok(Some((10, 19))));
        assert_eq!(wanted(Some("bytes=90-200"), 100), Ok(Some((90, 99))));
        assert_eq!(wanted(Some("bytes=-10"), 100), Ok(Some((90, 99))));
        assert_eq!(wanted(Some("bytes=100-"), 100), Err(()));
        assert_eq!(wanted(Some("bytes=5-2"), 100), Err(()));
    }

    async fn body(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    /// A video kept in a `.uxflight` is served from its place in the file,
    /// whole or a range of it, and none of the file around it.
    #[tokio::test]
    async fn a_video_in_a_flight_file_is_served_from_where_it_is_with_ranges() {
        use crate::flight_file::{Writer, entries, find};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.uxflight");
        let video: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut writer = Writer::create(&path, 0).unwrap();
        writer.add_bytes("flight.json", b"{}").unwrap();
        writer.add_bytes("video/a.mp4", &video).unwrap();
        writer.add_bytes("telemetry.tlog", b"after").unwrap();
        writer.finish().unwrap();
        let entry = find(&entries(&path).unwrap(), "video/a.mp4")
            .unwrap()
            .clone();
        let ask = |range: Option<&str>| {
            let mut request = Request::builder().uri("/recordings/a.mp4");
            if let Some(range) = range {
                request = request.header(header::RANGE, range);
            }
            request.body(Body::empty()).unwrap()
        };
        let whole = serve(
            path.clone(),
            entry.data_offset,
            entry.size,
            "video/mp4",
            asked(&ask(None)),
        )
        .await;
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(whole.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(body(whole).await, video);
        let part = serve(
            path.clone(),
            entry.data_offset,
            entry.size,
            "video/mp4",
            asked(&ask(Some("bytes=500000-500009"))),
        )
        .await;
        assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            part.headers()[header::CONTENT_RANGE],
            "bytes 500000-500009/1000000"
        );
        assert_eq!(part.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(body(part).await, &video[500_000..500_010]);
        let beyond = serve(
            path,
            entry.data_offset,
            entry.size,
            "video/mp4",
            asked(&ask(Some("bytes=1000000-"))),
        )
        .await;
        assert_eq!(beyond.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }
}
