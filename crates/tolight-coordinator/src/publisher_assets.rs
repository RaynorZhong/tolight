//! Static documentation preflight and installation, under the Publisher OS lock.
use super::{PublishError, TreeLock, atomic_write, sync_directory};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use tolight_core::signing;

/// Fixed, first-party brand images; never discover arbitrary files in public/.
pub const BRAND_ASSETS: [&str; 2] = ["long-shot-coffee.jpg", "long-shot-seal.jpg"];

pub fn referenced_brand_assets(index: &str) -> Vec<&'static str> {
    BRAND_ASSETS
        .into_iter()
        .filter(|name| index.contains(name))
        .collect()
}

/// Bounded JPEG container validation, not image decoding. Preserve source bytes.
pub fn valid_brand_jpeg(bytes: &[u8]) -> bool {
    if bytes.len() > 4 * 1024 * 1024 || !bytes.starts_with(&[0xff, 0xd8]) {
        return false;
    }
    let mut at = 2;
    let mut frame = false;
    let mut scan = false;
    while at < bytes.len() {
        if bytes[at] != 0xff {
            return false;
        }
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let Some(&marker) = bytes.get(at) else {
            return false;
        };
        at += 1;
        if marker == 0xd9 {
            return frame && scan && at == bytes.len();
        }
        if marker < 0xc0 || (0xd0..=0xd8).contains(&marker) {
            return false;
        }
        let Some(length) = bytes.get(at..at + 2) else {
            return false;
        };
        let length = u16::from_be_bytes([length[0], length[1]]) as usize;
        let Some(end) = at
            .checked_add(length)
            .filter(|end| length >= 2 && *end <= bytes.len())
        else {
            return false;
        };
        if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
            if length < 8 || bytes[at + 7] == 0 || length != 8 + 3 * bytes[at + 7] as usize {
                return false;
            }
            let height = u16::from_be_bytes([bytes[at + 3], bytes[at + 4]]);
            let width = u16::from_be_bytes([bytes[at + 5], bytes[at + 6]]);
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return false;
            }
            frame = true;
        }
        at = end;
        if marker == 0xda {
            if !frame
                || length < 6
                || bytes[end - length + 2] == 0
                || length != 6 + 2 * bytes[end - length + 2] as usize
            {
                return false;
            }
            scan = true;
            while at < bytes.len() {
                if bytes[at] != 0xff {
                    at += 1;
                } else if bytes
                    .get(at + 1)
                    .is_some_and(|next| *next == 0 || (0xd0..=0xd7).contains(next))
                {
                    at += 2;
                } else {
                    break;
                }
            }
        }
    }
    false
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SecurityAssetMetadata {
    pub contacts: Vec<String>,
    #[serde(with = "tolight_core::datetime::serde")]
    pub expires: time::OffsetDateTime,
    pub canonical: Vec<String>,
    pub source_path: PathBuf,
    pub policy_path: PathBuf,
}

fn invalid(path: &Path, detail: impl std::fmt::Display) -> PublishError {
    PublishError::Invalid(format!("{}: {detail}", path.display()))
}
fn read(path: &Path) -> Result<Vec<u8>, PublishError> {
    let bytes = fs::read(path).map_err(|e| invalid(path, e))?;
    if bytes.is_empty() {
        return Err(invalid(path, "文件为空"));
    }
    std::str::from_utf8(&bytes).map_err(|e| invalid(path, e))?;
    Ok(bytes)
}

fn contact_uri(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) || value.chars().any(char::is_control) {
        return false;
    }
    let Ok(uri) = url::Url::parse(value) else {
        return false;
    };
    if uri.fragment().is_some() || uri.password().is_some() || !uri.username().is_empty() {
        return false;
    }
    match uri.scheme() {
        "https" => uri.host_str().is_some(),
        "mailto" => {
            uri.query().is_none()
                && uri.path().split_once('@').is_some_and(|(name, domain)| {
                    !name.is_empty() && !domain.is_empty() && !domain.contains('@')
                })
        }
        "tel" => uri.query().is_none() && !uri.path().is_empty(),
        _ => false,
    }
}

fn https_uri(value: &str) -> Option<url::Url> {
    if !contact_uri(value) {
        return None;
    }
    url::Url::parse(value)
        .ok()
        .filter(|uri| uri.scheme() == "https")
}

fn security_document(
    source: &Path,
) -> Result<Option<(Vec<u8>, SecurityAssetMetadata)>, PublishError> {
    let directory = source.join(".well-known");
    if fs::symlink_metadata(&directory)
        .is_ok_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(invalid(&directory, "安全报告源目录须为常规目录"));
    }
    let path = source.join(".well-known/security.txt");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(invalid(&path, error)),
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(invalid(&path, "安全报告文件必须是常规文件"));
        }
        Ok(_) => {}
    }
    let bytes = read(&path)?;
    if bytes.len() > 65536 || !bytes.ends_with(b"\n") {
        return Err(invalid(
            &path,
            "security.txt 须为至多64KiB、每行换行的UTF-8文本",
        ));
    }
    let text = std::str::from_utf8(&bytes).expect("validated UTF-8");
    let mut contacts = Vec::new();
    let mut canonical = Vec::new();
    let mut expires = None;
    let mut languages = false;
    for line in text.lines() {
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(invalid(&path, "security.txt 含控制字符"));
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid(&path, "仅支持未签名RFC9116字段，每字段一行"))?;
        let value = value.trim();
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.is_empty()
        {
            return Err(invalid(&path, "security.txt 字段名称或值为空/无效"));
        }
        match name.to_ascii_lowercase().as_str() {
            "contact" => {
                if !contact_uri(value) {
                    return Err(invalid(&path, "Contact须为mailto、https或tel URI"));
                }
                contacts.push(value.to_owned());
            }
            "expires" => {
                if expires.is_some() {
                    return Err(invalid(&path, "Expires只能出现一次"));
                }
                let deadline = tolight_core::datetime::parse_rfc3339(value)
                    .map_err(|_| invalid(&path, "Expires须为RFC3339时间"))?;
                if deadline <= time::OffsetDateTime::now_utc() {
                    return Err(invalid(&path, "Expires已过期"));
                }
                expires = Some(deadline);
            }
            "canonical" => {
                let uri = https_uri(value)
                    .filter(|uri| {
                        uri.path() == "/.well-known/security.txt" && uri.query().is_none()
                    })
                    .ok_or_else(|| {
                        invalid(
                            &path,
                            "Canonical须为HTTPS的/.well-known/security.txt完整地址",
                        )
                    })?;
                canonical.push(uri.to_string());
            }
            "policy" | "acknowledgments" | "hiring" => {
                if https_uri(value).is_none() {
                    return Err(invalid(&path, "网页链接须为HTTPS URI"));
                }
            }
            "preferred-languages" => {
                if languages
                    || value.split(',').any(|tag| {
                        tag.trim().is_empty()
                            || !tag
                                .trim()
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
                {
                    return Err(invalid(&path, "Preferred-Languages须为一行非空语言标签"));
                }
                languages = true;
            }
            _ => {}
        }
    }
    if contacts.is_empty() {
        return Err(invalid(&path, "缺少必填Contact"));
    }
    let expires = expires.ok_or_else(|| invalid(&path, "缺少必填Expires"))?;
    let policy_path = source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("SECURITY.md");
    if fs::symlink_metadata(&policy_path)
        .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
    {
        return Err(invalid(&policy_path, "安全报告策略须为常规文件"));
    }
    let policy = read(&policy_path)?;
    let policy = std::str::from_utf8(&policy).expect("validated UTF-8");
    let policy_contacts: Vec<_> = policy
        .lines()
        .filter_map(|line| {
            line.trim()
                .split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("contact"))
                .map(|(_, value)| value.trim().to_owned())
        })
        .collect();
    if policy_contacts != contacts {
        return Err(invalid(
            &policy_path,
            "Contact字段必须与security.txt逐项一致（相同顺序）",
        ));
    }
    Ok(Some((
        bytes,
        SecurityAssetMetadata {
            contacts,
            expires,
            canonical,
            source_path: path,
            policy_path,
        },
    )))
}

pub(super) fn security_metadata(
    source: &Path,
) -> Result<Option<SecurityAssetMetadata>, PublishError> {
    security_document(source).map(|document| document.map(|(_, metadata)| metadata))
}

pub(super) fn install(
    root: &Path,
    source: &Path,
    docs: Option<&Path>,
    examples: Option<&Path>,
) -> Result<(), PublishError> {
    // Resolve every input and render every document before changing installed files.
    let mut files = Vec::<(String, Vec<u8>)>::new();
    for name in ["app.js", "style.css", "robots.txt", "index.html"] {
        files.push((name.into(), read(&source.join(name))?));
    }
    let index = std::str::from_utf8(&files[3].1).expect("validated UTF-8");
    let referenced = referenced_brand_assets(index);
    for name in BRAND_ASSETS {
        let path = source.join(name);
        if !referenced.contains(&name) && fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| invalid(&path, error))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > 4 * 1024 * 1024
        {
            return Err(invalid(&path, "品牌图片须为至多4MiB的常规JPEG文件"));
        }
        let bytes = fs::read(&path).map_err(|error| invalid(&path, error))?;
        if !valid_brand_jpeg(&bytes) {
            return Err(invalid(&path, "品牌图片的JPEG容器无效"));
        }
        // Install images before index.html, so new references never precede assets.
        files.insert(0, (name.into(), bytes));
    }
    let security = security_document(source)?;
    let remove_security = security.is_none();
    if let Some((bytes, _)) = security {
        files.push((".well-known/security.txt".into(), bytes));
    }
    if let (Some(docs), Some(examples)) = (docs, examples) {
        let join = read(&docs.join("join.md"))?;
        let vectors_path = docs.join("signing-vectors.json");
        let vectors = read(&vectors_path)?;
        validate_vectors(&vectors_path, &vectors)?;
        let python = read(&examples.join("minimal_client.py"))?;
        let mut documents = Vec::new();
        for name in ["participant-spec", "protocol", "rules"] {
            let path = docs.join(format!("{name}.md"));
            let markdown = read(&path)?;
            let rendered = render(
                &path,
                std::str::from_utf8(&markdown).expect("validated UTF-8"),
            )?;
            documents.push((format!("docs/{name}.html"), rendered.into_bytes()));
            // Preserve relative source links when an Agent follows the raw spec.
            documents.push((format!("docs/{name}.md"), markdown));
        }
        let index = files
            .iter_mut()
            .find(|(name, _)| name == "index.html")
            .expect("four assets");
        let mut text = String::from_utf8(std::mem::take(&mut index.1)).expect("validated UTF-8");
        let at = text.rfind("</footer>").ok_or_else(|| {
            invalid(
                &source.join("index.html"),
                "缺少 </footer>，无法安装接入链接",
            )
        })?;
        text.insert_str(at, "<a href=\"/join.md\">接入 tolight</a>");
        index.1 = text.into_bytes();
        // Documents precede the new footer, so a failed installation never advertises missing docs.
        documents.push(("join.md".into(), join));
        documents.push(("docs/signing-vectors.json".into(), vectors));
        documents.push(("examples/minimal_client.py".into(), python));
        documents.extend(files);
        files = documents;
    }

    let guard = TreeLock::acquire(root)?;
    let mut directories = BTreeSet::new();
    directories.insert(guard.root.clone());
    let security_directory = guard.root.join(".well-known");
    if let Ok(metadata) = fs::symlink_metadata(&security_directory) {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid(&security_directory, "输出目录须为常规目录"));
        }
        directories.insert(security_directory.clone());
    }
    for path in [
        security_directory.join("security.txt"),
        security_directory.join("security.txt.tmp"),
    ] {
        if fs::symlink_metadata(&path)
            .is_ok_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(invalid(&path, "安全报告输出文件不能是目录或符号链接"));
        }
    }
    // Validate all output paths before writing any; no asset path may alias data/.
    for (name, _) in &files {
        let path = guard.root.join(name);
        let directory = path.parent().expect("fixed relative asset");
        if fs::symlink_metadata(directory).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(invalid(directory, "输出目录不能是符号链接"));
        }
        for output in [
            path.clone(),
            path.with_file_name(format!(
                "{}.tmp",
                path.file_name().unwrap().to_string_lossy()
            )),
        ] {
            if fs::symlink_metadata(&output).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(invalid(&output, "输出文件不能是符号链接"));
            }
        }
        directories.insert(directory.to_owned());
    }
    for directory in &directories {
        fs::create_dir_all(directory)?;
    }
    let result = (|| {
        for (name, bytes) in &files {
            atomic_write(&guard.root.join(name), bytes)?;
        }
        if remove_security {
            match fs::remove_file(security_directory.join("security.txt")) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(PublishError::Io(error)),
            }
        }
        for directory in directories.iter().rev() {
            sync_directory(directory)?;
        }
        Ok(())
    })();
    // Known temporary names only: data/ and unrelated files are never visited.
    let cleanup = (|| {
        for (name, _) in &files {
            let path = guard.root.join(format!("{name}.tmp"));
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(PublishError::Io(error)),
            }
        }
        if remove_security {
            match fs::remove_file(security_directory.join("security.txt.tmp")) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(PublishError::Io(error)),
            }
        }
        for directory in directories.iter().rev() {
            sync_directory(directory)?;
        }
        Ok(())
    })();
    result.and(cleanup)
}

fn validate_vectors(path: &Path, bytes: &[u8]) -> Result<(), PublishError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| invalid(path, e))?;
    if value["schema"] != "tolight-signing-vectors/1" || value["scheme"] != "tolight-participant-v1"
    {
        return Err(invalid(path, "签名样例 schema/scheme 不符"));
    }
    let vectors = value["vectors"]
        .as_array()
        .filter(|v| v.len() >= 5)
        .ok_or_else(|| invalid(path, "vectors 须含至少五组签名样例"))?;
    let mut names = BTreeSet::new();
    for (index, vector) in vectors.iter().enumerate() {
        for field in [
            "name",
            "seed",
            "public_key",
            "method",
            "url",
            "authority",
            "host",
            "path",
            "query",
            "timestamp",
            "body_base64url",
            "body_sha256",
            "string_to_sign",
            "signature",
        ] {
            if !vector[field].is_string() {
                return Err(invalid(
                    path,
                    format!("vectors[{index}].{field} 须为字符串"),
                ));
            }
        }
        let string = |name: &str| vector[name].as_str().expect("validated field");
        if string("name").is_empty()
            || !names.insert(string("name"))
            || !matches!(string("method"), "GET" | "POST")
            || !string("path").starts_with("/participant/v1/")
            || string("string_to_sign").lines().count() != 7
            || string("body_sha256").len() != 64
            || !string("body_sha256")
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || string("timestamp").is_empty()
            || string("timestamp").len() > 12
            || !string("timestamp").bytes().all(|b| b.is_ascii_digit())
        {
            return Err(invalid(path, format!("vectors[{index}] 字段结构无效")));
        }
        for (field, length) in [("seed", 43), ("public_key", 43), ("signature", 86)] {
            if string(field).len() != length
                || !string(field)
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(invalid(path, format!("vectors[{index}].{field} 无效")));
            }
        }
        let fail = || invalid(path, format!("vectors[{index}] 请求或签名内容不一致"));
        let seed = signing::b64_decode::<32>(string("seed")).map_err(|_| fail())?;
        let body = URL_SAFE_NO_PAD
            .decode(string("body_base64url"))
            .map_err(|_| fail())?;
        if signing::b64_encode(&body) != string("body_base64url") {
            return Err(fail());
        }
        let url = url::Url::parse(string("url")).map_err(|_| fail())?;
        let raw_authority = string("url")
            .split_once("://")
            .ok_or_else(fail)?
            .1
            .split('/')
            .next()
            .ok_or_else(fail)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || raw_authority != string("authority")
            || url.path() != string("path")
            || url.query().unwrap_or("") != string("query")
        {
            return Err(fail());
        }
        let expected = signing::string_to_sign(
            string("method"),
            string("authority"),
            url.scheme(),
            string("path"),
            string("query"),
            string("timestamp"),
            &body,
        )
        .map_err(|_| fail())?;
        let headers = signing::sign_request(
            &seed,
            string("method"),
            string("authority"),
            url.scheme(),
            string("path"),
            string("query"),
            string("timestamp"),
            &body,
        )
        .map_err(|_| fail())?;
        if expected != string("string_to_sign")
            || expected.lines().nth(2) != Some(string("host"))
            || expected.lines().last() != Some(string("body_sha256"))
            || headers.key != string("public_key")
            || headers.signature != string("signature")
        {
            return Err(fail());
        }
        if let Some(authorities) = vector.get("equivalent_authorities") {
            let authorities = authorities.as_array().ok_or_else(fail)?;
            for authority in authorities {
                if signing::canonical_authority(authority.as_str().ok_or_else(fail)?, url.scheme())
                    .map_err(|_| fail())?
                    != string("host")
                {
                    return Err(fail());
                }
            }
        }
    }
    Ok(())
}

fn local_link(url: &str) -> Option<String> {
    let (path, fragment) = url.split_once('#').map_or((url, ""), |(p, f)| (p, f));
    if !fragment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return None;
    }
    let path = path.strip_prefix("https://tolight.com").unwrap_or(path);
    let target = match path {
        "" if !fragment.is_empty() => "",
        "join.md" | "/join.md" => "/join.md",
        "participant-spec.md"
        | "participant-spec.html"
        | "/docs/participant-spec.md"
        | "/docs/participant-spec.html" => "/docs/participant-spec.html",
        "protocol.md" | "protocol.html" | "/docs/protocol.md" | "/docs/protocol.html" => {
            "/docs/protocol.html"
        }
        "rules.md" | "rules.html" | "/docs/rules.md" | "/docs/rules.html" => "/docs/rules.html",
        "signing-vectors.json" | "/docs/signing-vectors.json" => "/docs/signing-vectors.json",
        "/examples/minimal_client.py" => "/examples/minimal_client.py",
        _ => return None,
    };
    Some(format!(
        "{target}{}",
        if fragment.is_empty() {
            String::new()
        } else {
            format!("#{fragment}")
        }
    ))
}
fn unsafe_url(url: &str) -> bool {
    url.starts_with("//")
        || url.contains('\\')
        || url.chars().any(char::is_control)
        || url.split_once(':').is_some_and(|(scheme, _)| {
            !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "mailto"
            )
        })
}
fn render(path: &Path, markdown: &str) -> Result<String, PublishError> {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    );
    let mut events = Vec::new();
    let mut links = Vec::new();
    for event in parser {
        match event {
            Event::Html(_) | Event::InlineHtml(_) => {
                return Err(invalid(path, "Markdown 不允许 raw HTML"));
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                if unsafe_url(&dest_url) {
                    return Err(invalid(path, "Markdown 不允许危险链接 URL"));
                }
                let destination = local_link(&dest_url);
                links.push(destination.is_some());
                if let Some(destination) = destination {
                    events.push(Event::Start(Tag::Link {
                        link_type,
                        dest_url: destination.into(),
                        title,
                        id,
                    }));
                } else {
                    events.push(Event::Start(Tag::Emphasis));
                }
            }
            Event::End(TagEnd::Link) => events.push(Event::End(
                if links.pop().expect("balanced parser events") {
                    TagEnd::Link
                } else {
                    TagEnd::Emphasis
                },
            )),
            Event::Start(Tag::Image { dest_url, .. }) => {
                if unsafe_url(&dest_url) {
                    return Err(invalid(path, "Markdown 不允许危险图片 URL"));
                }
                events.push(Event::Start(Tag::Emphasis));
            }
            Event::End(TagEnd::Image) => events.push(Event::End(TagEnd::Emphasis)),
            event => events.push(event),
        }
    }
    let mut output = String::from(
        "<!doctype html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>tolight 文档</title></head><body><nav><a href=\"/\">聊天室</a> · <a href=\"/join.md\">接入指南</a> · <a href=\"/docs/participant-spec.html\">参与者规范</a> · <a href=\"/docs/protocol.html\">公共格式</a> · <a href=\"/docs/rules.html\">接入规则</a></nav><main>\n",
    );
    html::push_html(&mut output, events.into_iter());
    output.push_str("</main></body></html>\n");
    Ok(output)
}
