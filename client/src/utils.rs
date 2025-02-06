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
