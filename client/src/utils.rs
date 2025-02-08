use anyhow::{anyhow, Result};
use common::protocol::HttpData;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Version,
};

pub async fn to_reqwest(client: &reqwest::Client, request: HttpData) -> Result<reqwest::Request> {
    let HttpData::Request {
        method,
        url,
        headers,
        body,
        version,
    } = request
    else {
        return Err(anyhow!("Expected an HTTP request"));
    };
    let method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
    let version = match version {
        0 => Version::HTTP_10,
        1 => Version::HTTP_11,
        2 => Version::HTTP_2,
        3 => Version::HTTP_3,
        _ => Version::HTTP_11,
    };

    let mut header_map = HeaderMap::new();
    for (key, value) in headers {
        header_map.insert(
            HeaderName::from_bytes(key.as_bytes())?,
            HeaderValue::from_bytes(value.as_bytes())?,
        );
    }

    Ok(client
        .request(method, url)
        .version(version)
        .headers(header_map)
        .body(body)
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
