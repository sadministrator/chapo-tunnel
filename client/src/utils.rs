use anyhow::Result;
use common::protocol::HttpRequest;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Method, Version,
};

pub async fn to_reqwest(
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
        .body(request.body)
        .build()?)
}
