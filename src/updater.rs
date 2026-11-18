//! CompactRS fork-only updater.
//! Fail closed: compare structured versions, require GitHub's SHA256 digest,
//! verify all downloaded bytes before touching the currently running EXE.
use crate::types::*;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

const API: &str = "https://api.github.com/repos/rampageX/compactrs/releases/latest";
const ASSET_PREFIX: &str = "https://github.com/rampageX/compactrs/releases/download/";
const MAX_RELEASE_JSON: usize = 2 * 1024 * 1024;
const MAX_BINARY: usize = 128 * 1024 * 1024;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(a: LPCWSTR, b: u32, c: LPCWSTR, d: LPCWSTR, e: u32) -> *mut c_void;
    fn WinHttpConnect(a: *mut c_void, b: LPCWSTR, c: u16, d: u32) -> *mut c_void;
    fn WinHttpOpenRequest(a: *mut c_void, b: LPCWSTR, c: LPCWSTR, d: LPCWSTR, e: LPCWSTR, f: *const *const u16, g: u32) -> *mut c_void;
    fn WinHttpSendRequest(a: *mut c_void, b: LPCWSTR, c: u32, d: *const c_void, e: u32, f: u32, g: usize) -> i32;
    fn WinHttpReceiveResponse(a: *mut c_void, b: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(a: *mut c_void, b: u32, c: LPCWSTR, d: *mut c_void, e: *mut u32, f: *mut u32) -> i32;
    fn WinHttpReadData(a: *mut c_void, b: *mut c_void, c: u32, d: *mut u32) -> i32;
    fn WinHttpCloseHandle(a: *mut c_void) -> i32;
}

// CryptoAPI SHA-256 is part of Windows 7+; no external Rust crates or DLLs.
#[link(name = "advapi32")]
unsafe extern "system" {
    fn CryptAcquireContextW(provider: *mut usize, container: LPCWSTR, provider_name: LPCWSTR, kind: u32, flags: u32) -> i32;
    fn CryptCreateHash(provider: usize, algorithm: u32, key: usize, flags: u32, hash: *mut usize) -> i32;
    fn CryptHashData(hash: usize, data: *const u8, length: u32, flags: u32) -> i32;
    fn CryptGetHashParam(hash: usize, parameter: u32, data: *mut u8, len: *mut u32, flags: u32) -> i32;
    fn CryptDestroyHash(hash: usize) -> i32;
    fn CryptReleaseContext(provider: usize, flags: u32) -> i32;
}

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { WinHttpCloseHandle(self.0); }
        }
    }
}
struct Link { _session: Handle, _connection: Handle, request: Handle }

fn to_w(s: &str) -> Vec<u16> { crate::utils::to_wstring(s) }

fn get(url: &str) -> Result<Link, &'static str> {
    if !url.starts_with("https://") { return Err("Only HTTPS updates are allowed"); }
    let session = Handle(unsafe { WinHttpOpen(crate::w!("CompactRS-rampageX/1.0").as_ptr(), 0, null(), null(), 0) });
    if session.0.is_null() { return Err("HTTP initialization failed"); }

    // WinHTTP redirects GitHub release asset requests to GitHub's asset CDN.
    // All redirects are HTTPS; no insecure fallback is ever attempted.
    let endpoint = url.strip_prefix("https://").ok_or("Invalid HTTPS URL")?;
    let (host, path) = endpoint.split_once('/').ok_or("Invalid update URL")?;
    if host.is_empty() { return Err("Missing update host"); }
    let connection = Handle(unsafe { WinHttpConnect(session.0, to_w(host).as_ptr(), 443, 0) });
    if connection.0.is_null() { return Err("HTTP connection failed"); }
    let request = Handle(unsafe {
        WinHttpOpenRequest(connection.0, null(), to_w(&format!("/{}", path)).as_ptr(),
            null(), null(), null(), 0x0080_0000)
    });
    if request.0.is_null() { return Err("HTTP request failed"); }

    let headers = crate::w!("Accept: application/vnd.github+json\r\n");
    if unsafe { WinHttpSendRequest(request.0, headers.as_ptr(), (headers.len()-1) as u32, null(), 0, 0, 0) } == 0 {
        return Err("HTTP send failed");
    }
    if unsafe { WinHttpReceiveResponse(request.0, null_mut()) } == 0 {
        return Err("HTTP response failed");
    }
    let (mut status, mut len) = (0u32, 4u32);
    if unsafe { WinHttpQueryHeaders(request.0, 0x2000_0013, null(), &mut status as *mut _ as _, &mut len, null_mut()) } == 0 {
        return Err("Cannot read HTTP status");
    }
    if status != 200 { return Err("GitHub HTTP request was not successful"); }
    Ok(Link { _session: session, _connection: connection, request })
}

fn read_response(link: &Link, max: usize) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut count = 0u32;
        if unsafe { WinHttpReadData(link.request.0, chunk.as_mut_ptr() as _, chunk.len() as u32, &mut count) } == 0 {
            return Err("Download interrupted");
        }
        if count == 0 { break; }
        if bytes.len().saturating_add(count as usize) > max { return Err("Download exceeds size limit"); }
        bytes.extend_from_slice(&chunk[..count as usize]);
    }
    if bytes.is_empty() { return Err("Empty download"); }
    Ok(bytes)
}

// Read an unescaped JSON string. GitHub's tag, URLs, asset names and digest
// are ASCII; reject JSON escape sequences except an escaped slash.
fn json_string_at(s: &str, mut index: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    while index < b.len() && b[index].is_ascii_whitespace() { index+=1; }
    if b.get(index) != Some(&b'"') { return None; }
    index+=1;
    let mut result = String::new();
    while index < b.len() {
        match b[index] {
            b'"' => return Some((result, index+1)),
            b'\\' => {
                index+=1;
                match b.get(index)? {
                    b'/' | b'\\' | b'"' => result.push(*b.get(index)? as char),
                    _ => return None,
                }
            },
            c if c < 0x80 => result.push(c as char),
            _ => return None,
        }
        index+=1;
    }
    None
}

fn json_field(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let mut offset = 0;
    while let Some(pos) = s[offset..].find(&needle) {
        let mut cursor = offset + pos + needle.len();
        let bytes = s.as_bytes();
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() { cursor+=1; }
        if bytes.get(cursor) == Some(&b':') {
            if let Some((value, _)) = json_string_at(s, cursor+1) { return Some(value); }
        }
        offset = cursor;
    }
    None
}

// Find asset objects using string-aware brace balancing; do not match an
// "compactrs.exe" substring in unrelated release notes or nested uploader data.
fn asset_object(json: &str) -> Option<&str> {
    let pos = json.find("\"assets\"")?;
    let rest = &json[pos+8..];
    let start = rest.find('[')? + pos + 8;
    let b = json.as_bytes();
    let mut i = start+1;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') { i+=1; }
        if b.get(i) == Some(&b']') { break; }
        if b.get(i) != Some(&b'{') { return None; }
        let begin = i;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        while i < b.len() {
            let c = b[i];
            if in_string {
                if escaped { escaped = false; }
                else if c == b'\\' { escaped = true; }
                else if c == b'"' { in_string = false; }
            } else if c == b'"' {
                in_string = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 { break; }
            }
            i+=1;
        }
        if depth != 0 { return None; }
        let item = &json[begin..=i];
        if json_field(item, "name").as_deref() == Some("compactrs.exe") {
            return Some(item);
        }
        i+=1;
    }
    None
}

fn parse_version(input: &str) -> Option<(u32,u32,u32,u32)> {
    let value = input.strip_prefix('v').unwrap_or(input);
    let (date, revision) = value.split_once("-r")?;
    let mut date = date.split('.');
    let (year, month, day) = (date.next()?.parse().ok()?,date.next()?.parse().ok()?,date.next()?.parse().ok()?);
    if date.next().is_some() { return None; }
    let clock: u32 = revision.parse().ok()?;
    if revision.len()!=4 || month==0 || month>12 || day==0 || day>31 || clock/100>23 || clock%100>59 {
        return None;
    }
    Some((year,month,day,clock))
}

fn valid_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len()==64 && hex.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub download_url: String,
    pub sha256: String,
}

fn parse_update_info(json: &str, current: &str) -> Result<Option<UpdateInfo>, &'static str> {
    let latest = json_field(json, "tag_name").ok_or("Release tag missing")?;
    let remote = parse_version(&latest).ok_or("Invalid release version")?;
    let local = parse_version(current).ok_or("Invalid installed version")?;
    if remote <= local { return Ok(None); }
    let asset = asset_object(json).ok_or("EXE asset not found")?;
    let url = json_field(asset, "browser_download_url").ok_or("Asset URL missing")?;
    if !url.starts_with(ASSET_PREFIX) || !url.ends_with("/compactrs.exe")
        || !url.contains(&format!("/{}/",latest)) {
        return Err("Untrusted release asset URL");
    }
    let sha256 = valid_digest(&json_field(asset, "digest").ok_or("Release SHA256 unavailable")?)
        .ok_or("Invalid release SHA256")?;
    Ok(Some(UpdateInfo { version: latest, download_url: url, sha256 }))
}

pub fn check_for_updates() -> Result<Option<UpdateInfo>, &'static str> {
    let response = get(API)?;
    let bytes = read_response(&response, MAX_RELEASE_JSON)?;
    let json = std::str::from_utf8(&bytes).map_err(|_|"Malformed release response")?;
    parse_update_info(json, env!("APP_VERSION"))
}

fn sha256(data: &[u8]) -> Result<String, &'static str> {
    let mut provider = 0usize;
    if unsafe { CryptAcquireContextW(&mut provider, null(), null(), 24, 0xF000_0000) } == 0 {
        return Err("SHA256 provider unavailable");
    }
    let mut hash = 0usize;
    let result = (|| {
        if unsafe { CryptCreateHash(provider, 0x0000_800c, 0, 0, &mut hash) } == 0 {
            return Err("SHA256 initialization failed");
        }
        for chunk in data.chunks(1024*1024) {
            if unsafe { CryptHashData(hash, chunk.as_ptr(), chunk.len() as u32, 0) } == 0 {
                return Err("SHA256 processing failed");
            }
        }
        let (mut output, mut len) = ([0u8;32],32u32);
        if unsafe { CryptGetHashParam(hash, 2, output.as_mut_ptr(), &mut len, 0) } == 0 || len!=32 {
            return Err("SHA256 finalization failed");
        }
        let mut text = String::with_capacity(64);
        for byte in output { text.push_str(&format!("{:02x}", byte)); }
        Ok(text)
    })();
    unsafe {
        if hash != 0 { CryptDestroyHash(hash); }
        CryptReleaseContext(provider, 0);
    }
    result
}

pub fn download_and_start_update(info: &UpdateInfo) -> Result<(), &'static str> {
    if !info.download_url.starts_with(ASSET_PREFIX)
        || !info.download_url.ends_with("/compactrs.exe")
        || !info.download_url.contains(&format!("/{}/", info.version))
        || valid_digest(&format!("sha256:{}", info.sha256)).is_none() {
        return Err("Untrusted update request");
    }
    let response = get(&info.download_url)?;
    let bytes = read_response(&response, MAX_BINARY)?;
    if bytes.len() < 2 || &bytes[..2] != b"MZ" { return Err("Downloaded file is not a PE executable"); }
    if sha256(&bytes)? != info.sha256 { return Err("SHA256 mismatch — current EXE was not changed"); }

    // Everything above must succeed before modifying the current installation.
    let exe = std::env::current_exe().map_err(|_|"Executable path unavailable")?;
    let tmp = exe.with_extension("exe.tmp");
    let old = exe.with_extension("exe.old");
    {
        use std::io::Write;
        let mut out = std::fs::File::create(&tmp).map_err(|_|"Cannot create update temporary file")?;
        if out.write_all(&bytes).is_err() || out.sync_all().is_err() {
            drop(out);
            let _ = std::fs::remove_file(&tmp);
            return Err("Failed to save verified update");
        }
    }
    if old.exists() && std::fs::remove_file(&old).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err("Cannot replace previous .old backup");
    }
    let exe_w = to_w(&exe.to_string_lossy());
    let old_w = to_w(&old.to_string_lossy());
    let tmp_w = to_w(&tmp.to_string_lossy());
    unsafe {
        if MoveFileExW(exe_w.as_ptr(), old_w.as_ptr(), 1) == 0 {
            let _ = std::fs::remove_file(&tmp);
            return Err("Cannot back up running EXE");
        }
        if MoveFileExW(tmp_w.as_ptr(), exe_w.as_ptr(), 1) == 0 {
            if MoveFileExW(old_w.as_ptr(), exe_w.as_ptr(), 1) == 0 {
                return Err("Update failed; automatic rollback failed — .old is retained");
            }
            let _ = std::fs::remove_file(&tmp);
            return Err("Update failed; original EXE restored");
        }
    }
    Ok(())
}

#[cfg(feature = "shell-broker-smoke")]
pub fn updater_selftest() -> bool {
    // CI-only: real Windows CryptoAPI, version ordering, and JSON asset parsing.
    let fake = r#"{"tag_name":"v2026.11.19-r0912","assets":[{"name":"compactrs.exe","browser_download_url":"https://github.com/rampageX/compactrs/releases/download/v2026.11.19-r0912/compactrs.exe","digest":"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}]}"#;
    let newer = parse_update_info(fake, "v2026.11.18-r2145")
        .ok().flatten().map(|x| x.sha256=="ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap_or(false);
    let older = matches!(parse_update_info(fake, "v2026.11.20-r0001"), Ok(None));
    let same = matches!(parse_update_info(fake, "v2026.11.19-r0912"), Ok(None));
    let malformed = parse_version("v2026.13.99-r2561").is_none();
    let expected_hash = sha256(b"abc").map(|x| x=="ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap_or(false);
    newer && older && same && malformed && expected_hash
}

#[cfg(feature = "shell-broker-smoke")]
pub fn updater_live_selftest() -> Result<(), &'static str> {
    // Real GitHub API and asset download, but no executable is ever replaced.
    let release = get(API)?;
    let response = read_response(&release, MAX_RELEASE_JSON)?;
    let json = std::str::from_utf8(&response).map_err(|_|"Malformed GitHub UTF-8")?;
    let info = parse_update_info(json, "v2020.1.1-r0000")?
        .ok_or("Expected a later fork release")?;
    let asset = get(&info.download_url)?;
    let exe = read_response(&asset, MAX_BINARY)?;
    if exe.len() < 2 || &exe[..2] != b"MZ" {
        return Err("GitHub asset missing PE header");
    }
    if sha256(&exe)? != info.sha256 {
        return Err("Actual GitHub asset SHA256 does not match the release digest");
    }
    Ok(())
}
