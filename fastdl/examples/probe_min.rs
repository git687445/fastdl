use std::error::Error;

fn main() {
    let client = reqwest::blocking::Client::new();
    match client.get("http://127.0.0.1:18472/x").header("Range", "bytes=0-0").send() {
        Ok(resp) => println!("range: OK status={} TE={:?}", resp.status(),
            resp.headers().get("transfer-encoding").map(|v| v.to_str().ok())),
        Err(e) => {
            let mut chain = vec![e.to_string()];
            let mut src = e.source();
            while let Some(c) = src { chain.push(c.to_string()); src = c.source(); }
            println!("range: ERR chain = [{}]", chain.join(" <- "));
        }
    }
    match client.get("http://127.0.0.1:18472/x").send() {
        Ok(resp) => println!("plain: OK status={}", resp.status()),
        Err(e) => {
            let mut chain = vec![e.to_string()];
            let mut src = e.source();
            while let Some(c) = src { chain.push(c.to_string()); src = c.source(); }
            println!("plain: ERR chain = [{}]", chain.join(" <- "));
        }
    }
}
