use futures::StreamExt;

pub(crate) struct LimitedBody {
    pub status: reqwest::StatusCode,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(crate) async fn read_response(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<LimitedBody, String> {
    let status = response.status();
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(max_bytes),
    );
    let mut stream = response.bytes_stream();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("read HTTP response: {error}"))?;
        let remaining = max_bytes.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == max_bytes {
            if stream
                .next()
                .await
                .transpose()
                .map_err(|error| format!("read HTTP response after size boundary: {error}"))?
                .is_some()
            {
                truncated = true;
            }
            break;
        }
    }

    Ok(LimitedBody {
        status,
        bytes,
        truncated,
    })
}
