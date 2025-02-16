use anyhow::Result;
use common::protocol::HttpRequest;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Version,
};

pub async fn to_reqwest_header(
    client: &reqwest::Client,
    request: HttpRequest,
) -> Result<reqwest::Request> {
    let method = Method::from_bytes(request.method.as_bytes()).unwrap_or(Method::GET);
    let version = match request.version {
        0 => Version::HTTP_10,
        1 => Version::HTTP_11,
        2 => Version::HTTP_2,
        3 => Version::HTTP_3,
        _ => Version::HTTP_11,
    };

    let mut headers = HeaderMap::new();
    for (key, value) in request.headers {
        headers.insert(
            HeaderName::from_bytes(key.as_bytes())?,
            HeaderValue::from_bytes(value.as_bytes())?,
        );
    }

    Ok(client
        .request(method, request.url)
        .version(version)
        .headers(headers)
        .build()?)
}

pub fn decode_url(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if c == '%' {
            if let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
                if let Ok(byte) = u8::from_str_radix(&format!("{}{}", c1, c2), 16) {
                    if let Some(decoded_char) = char::from_u32(byte as u32) {
                        output.push(decoded_char);
                        continue;
                    }
                }
            }
            output.push('%');
        } else {
            output.push(c);
        }
    }

    output
}

pub fn is_video_request(headers: &std::collections::HashMap<String, String>, url: &str) -> bool {
    headers.get("Accept").map_or(false, |accept| {
        accept.contains("video/")
            || url.ends_with(".mp4")
            || url.ends_with(".webm")
            || url.ends_with(".mov")
    }) || headers
        .get("Sec-Fetch-Dest")
        .map_or(false, |dest| dest == "video")
}
