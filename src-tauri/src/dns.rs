//! Проверка настроек DNS («Проверить» на странице настроек). Порт `src/main/dns.ts`.
//!
//! Системный резолвер, прямые UDP-запросы к серверам и DNS-over-HTTPS (RFC 8484).

use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::errors::{AppError, AppResult};

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const DOH_TIMEOUT: Duration = Duration::from_secs(6);
const PLAIN_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_DNS_SERVERS: usize = 4;

/// Minimal DNS query: id 0, recursion desired, one question (RFC 8484 §4.1).
pub fn encode_query(name: &str, qtype: u16) -> AppResult<Vec<u8>> {
    let mut out = vec![0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    let trimmed = name.strip_suffix('.').unwrap_or(name);
    if trimmed.is_empty() {
        return Err(AppError::new("EBADNAME", "Пустое имя хоста"));
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > 63 || !label.is_ascii() {
            return Err(AppError::new("EBADNAME", format!("Некорректное имя хоста: {name}")));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.extend_from_slice(&[0, (qtype >> 8) as u8, qtype as u8, 0, 1]);
    Ok(out)
}

fn read_u16(msg: &[u8], offset: usize) -> AppResult<u16> {
    let bytes = msg
        .get(offset..offset + 2)
        .ok_or_else(|| AppError::new("EBADRESP", "DNS: повреждённый ответ"))?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(msg: &[u8], offset: usize) -> AppResult<u32> {
    let bytes = msg
        .get(offset..offset + 4)
        .ok_or_else(|| AppError::new("EBADRESP", "DNS: повреждённый ответ"))?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn skip_name(msg: &[u8], offset: &mut usize) -> AppResult<()> {
    loop {
        let len = *msg
            .get(*offset)
            .ok_or_else(|| AppError::new("EBADRESP", "DNS: повреждённый ответ"))?;
        if len == 0 {
            *offset += 1;
            return Ok(());
        }
        if len & 0xc0 == 0xc0 {
            *offset += 2;
            return Ok(());
        }
        *offset += 1 + len as usize;
        if *offset > msg.len() {
            return Err(AppError::new("EBADRESP", "DNS: повреждённый ответ"));
        }
    }
}

/// Returns `(rcode, addresses, ttl)`.
pub fn decode_answer(msg: &[u8]) -> AppResult<(u8, Vec<String>, u32)> {
    if msg.len() < 12 {
        return Err(AppError::new("EBADRESP", "DNS: слишком короткий ответ"));
    }
    let rcode = (read_u16(msg, 2)? & 0x0f) as u8;
    let questions = read_u16(msg, 4)?;
    let answers = read_u16(msg, 6)?;
    let mut offset = 12usize;
    for _ in 0..questions {
        skip_name(msg, &mut offset)?;
        offset += 4;
    }
    let mut addresses = Vec::new();
    let mut ttl = u32::MAX;
    for _ in 0..answers {
        skip_name(msg, &mut offset)?;
        if offset + 10 > msg.len() {
            return Err(AppError::new("EBADRESP", "DNS: повреждённый ответ"));
        }
        let kind = read_u16(msg, offset)?;
        let record_ttl = read_u32(msg, offset + 4)?;
        let len = read_u16(msg, offset + 8)? as usize;
        offset += 10;
        if offset + len > msg.len() {
            return Err(AppError::new("EBADRESP", "DNS: повреждённый ответ"));
        }
        if kind == TYPE_A && len == 4 {
            let bytes = &msg[offset..offset + 4];
            addresses.push(format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]));
            ttl = ttl.min(record_ttl);
        } else if kind == TYPE_AAAA && len == 16 {
            let groups: Vec<String> = (0..8)
                .map(|index| format!("{:x}", read_u16(msg, offset + index * 2).unwrap_or(0)))
                .collect();
            addresses.push(groups.join(":"));
            ttl = ttl.min(record_ttl);
        }
        offset += len;
    }
    Ok((rcode, addresses, if ttl == u32::MAX { 30 } else { ttl }))
}

fn parse_servers(text: &str) -> Option<Vec<IpAddr>> {
    let list: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .filter(|part| !part.is_empty())
        .collect();
    if list.is_empty() || list.len() > MAX_DNS_SERVERS {
        return None;
    }
    let mut unique = Vec::new();
    for item in list {
        let Ok(address) = item.parse::<IpAddr>() else { return None };
        if !unique.contains(&address) {
            unique.push(address);
        }
    }
    Some(unique)
}

fn valid_doh_url(url: &str) -> bool {
    url.len() <= 256
        && url.starts_with("https://")
        && !url.contains('@')
        && !url.contains('#')
        && url[8..].split('/').next().is_some_and(|host| !host.is_empty())
}

fn udp_query(server: IpAddr, query: &[u8]) -> AppResult<Vec<u8>> {
    let target = match server {
        IpAddr::V4(address) => SocketAddr::from((address, 53)),
        IpAddr::V6(address) => format!("[{address}]:53")
            .parse::<SocketAddr>()
            .map_err(|error| AppError::new("DNS_FAILED", error.to_string()))?,
    };
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(PLAIN_TIMEOUT))?;
    socket.send_to(query, target)?;
    let mut buffer = [0u8; 4096];
    let (size, _) = socket.recv_from(&mut buffer)?;
    Ok(buffer[..size].to_vec())
}

async fn doh_query(url: &str, query: &[u8]) -> AppResult<Vec<u8>> {
    let response = reqwest::Client::builder()
        .timeout(DOH_TIMEOUT)
        .build()?
        .post(url)
        .header("Content-Type", "application/dns-message")
        .header("Accept", "application/dns-message")
        .body(query.to_vec())
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(AppError::new("DNS_FAILED", format!("DoH вернул {}", response.status())));
    }
    Ok(response.bytes().await?.to_vec())
}

fn result(host: &str, started: Instant, addresses: Vec<String>, error: Option<String>) -> Value {
    json!({
        "ok": error.is_none(),
        "host": host,
        "addresses": addresses.into_iter().take(6).collect::<Vec<_>>(),
        "ms": started.elapsed().as_millis() as u64,
        "error": error,
    })
}

/// `net:testDns`. Never changes the active resolver.
pub async fn test_dns(mode: &str, servers: &str, doh_url: &str) -> AppResult<Value> {
    let host = "api.modrinth.com";
    let started = Instant::now();

    let addresses = match mode {
        "system" => {
            let resolved = (host, 0u16)
                .to_socket_addrs()
                .map_err(|error| AppError::new("DNS_FAILED", error.to_string()))?
                .map(|address| address.ip().to_string())
                .collect::<Vec<_>>();
            if resolved.is_empty() {
                return Ok(result(host, started, Vec::new(), Some("адреса не найдены".into())));
            }
            resolved
        }
        "plain" => {
            let list = parse_servers(servers)
                .ok_or_else(|| AppError::new("DNS_FAILED", "Некорректный список DNS-серверов"))?;
            let mut found = Vec::new();
            let mut last_error = None;
            for server in list {
                for qtype in [TYPE_A, TYPE_AAAA] {
                    let query = encode_query(host, qtype)?;
                    match udp_query(server, &query).and_then(|bytes| decode_answer(&bytes)) {
                        Ok((code, mut addresses, _)) if code == 0 && !addresses.is_empty() => {
                            found.append(&mut addresses);
                        }
                        Ok((code, _, _)) => last_error = Some(format!("DNS-сервер вернул код {code}")),
                        Err(error) => last_error = Some(error.message),
                    }
                }
                if !found.is_empty() {
                    break;
                }
            }
            if found.is_empty() {
                return Ok(result(
                    host,
                    started,
                    Vec::new(),
                    Some(last_error.unwrap_or_else(|| "DNS-сервер не отвечает".into())),
                ));
            }
            found
        }
        "doh" => {
            if !valid_doh_url(doh_url) {
                return Err(AppError::new("DNS_FAILED", "Некорректный DoH-адрес: нужен https:// URL"));
            }
            let mut found = Vec::new();
            for qtype in [TYPE_A, TYPE_AAAA] {
                let query = encode_query(host, qtype)?;
                let bytes = doh_query(doh_url, &query).await?;
                let (code, mut addresses, _) = decode_answer(&bytes)?;
                if code != 0 {
                    return Ok(result(host, started, Vec::new(), Some(format!("DoH вернул код {code}"))));
                }
                found.append(&mut addresses);
            }
            found
        }
        other => {
            return Err(AppError::new("INVALID_ARGS", format!("Неизвестный режим DNS: {other}")));
        }
    };

    if addresses.is_empty() {
        return Ok(result(host, started, Vec::new(), Some("адреса не найдены".into())));
    }
    Ok(result(host, started, addresses, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_query() {
        let query = encode_query("api.modrinth.com", TYPE_A).unwrap();
        assert_eq!(&query[..12], &[0, 0, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(query[12], 3);
        assert_eq!(&query[13..16], b"api");
        assert_eq!(&query[query.len() - 5..], &[0, 0, 1, 0, 1]);
        assert!(encode_query("../bad", TYPE_A).is_err());
    }

    #[test]
    fn decodes_an_answer() {
        // one A record for 1.1.1.1, TTL 60
        let mut msg = vec![0, 0, 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
        msg.extend_from_slice(&[7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0, 1, 0, 1]);
        msg.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 1, 1, 1, 1]);
        let (code, addresses, ttl) = decode_answer(&msg).unwrap();
        assert_eq!(code, 0);
        assert_eq!(addresses, vec!["1.1.1.1"]);
        assert_eq!(ttl, 60);
    }

    #[test]
    fn parses_servers_and_doh_urls() {
        assert_eq!(parse_servers("1.1.1.1, 8.8.8.8").unwrap().len(), 2);
        assert!(parse_servers("1.1.1.1, 999.1.1.1").is_none());
        assert!(parse_servers("").is_none());
        assert!(valid_doh_url("https://cloudflare-dns.com/dns-query"));
        assert!(!valid_doh_url("http://cloudflare-dns.com/dns-query"));
        assert!(!valid_doh_url("https://user@cloudflare-dns.com/dns-query"));
    }
}
