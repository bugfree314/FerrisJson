//! cargo run --release --example generate_json -- 37kb output.json [seed]
use std::{
    collections::hash_map::RandomState,
    env,
    fs::OpenOptions,
    hash::BuildHasher,
    io::{self, BufWriter, Write},
    process::ExitCode,
    time::{Instant, SystemTime},
};

const PREFIX: &str = "{\"types\":{\"string\":\"你好 JSON 🦀\\n\\\"quoted\\\"\",\"integer\":42,\"negative\":-7,\"float\":3.14159,\"boolean\":true,\"null\":null,\"array\":[1,\"two\",false,null],\"object\":{\"nested\":true},\"empty_array\":[],\"empty_object\":{}},\"records\":[";
const SUFFIX: &str = "],\"padding\":\"";
const END: &str = "\"}\n";

// SplitMix64: reproducible test data, not a cryptographic random generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn string(&mut self, len: usize) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| ALPHABET[(self.next() % ALPHABET.len() as u64) as usize] as char)
            .collect()
    }

    fn record(&mut self, id: u64) -> String {
        let len = 16 + (self.next() % 113) as usize;
        let name = self.string(len);
        let integer = self.next() as i64;
        let float = (self.next() % 2_000_001) as f64 / 100.0 - 10_000.0;
        let active = self.next() & 1 == 1;
        let nullable = if self.next() & 1 == 1 {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(self.string(12))
        };
        serde_json::json!({
            "id": id,
            "name": name,
            "integer": integer,
            "float": float,
            "active": active,
            "optional": nullable,
            "items": [self.next() % 1000, "text", active, null, {"child": self.next() % 100}],
            "metadata": {"label": self.string(8), "tags": ["rust", "json"], "empty": []}
        })
        .to_string()
    }
}

fn parse_size(input: &str) -> Result<u64, String> {
    let input = input.trim();
    let split = input
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(input.len());
    let (number, unit) = input.split_at(split);
    let multiplier: u128 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return Err(format!("Unknown size unit: {unit}")),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 9
    {
        return Err(
            "Use a positive decimal size with at most 9 decimal places, e.g. 37kb or 1.06GB".into(),
        );
    }
    let scale = 10u128.pow(fraction.len() as u32);
    let digits = format!("{whole}{fraction}")
        .parse::<u128>()
        .map_err(|_| "Size is too large")?;
    let scaled = digits.checked_mul(multiplier).ok_or("Size is too large")?;
    // Round fractional bytes to the nearest whole byte, using integer arithmetic.
    let bytes = scaled / scale + u128::from(scaled % scale >= scale.div_ceil(2));
    let bytes = u64::try_from(bytes).map_err(|_| "Size is too large")?;
    if bytes == 0 {
        return Err("Size must be greater than zero".into());
    }
    Ok(bytes)
}

fn minimum_size() -> u64 {
    (PREFIX.len() + SUFFIX.len() + END.len()) as u64
}

fn generate(writer: &mut impl Write, size: u64, seed: u64) -> io::Result<u64> {
    let mut remaining = size.checked_sub(minimum_size()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Minimum size is {} bytes", minimum_size()),
        )
    })?;
    let mut rng = Rng(seed);
    let mut count = 0;
    writer.write_all(PREFIX.as_bytes())?;
    loop {
        let record = rng.record(count);
        let needed = record.len() as u64 + u64::from(count != 0);
        if needed > remaining {
            break;
        }
        if count != 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(record.as_bytes())?;
        remaining -= needed;
        count += 1;
    }
    writer.write_all(SUFFIX.as_bytes())?;
    // Only the small remainder is padding; almost all bytes are structured records.
    while remaining > 0 {
        let len = remaining.min(4096) as usize;
        writer.write_all(rng.string(len).as_bytes())?;
        remaining -= len as u64;
    }
    writer.write_all(END.as_bytes())?;
    Ok(count)
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!(
            "Usage: generate_json <size> <output.json> [seed]\n\
                  Units: B, KB, MB, GB, TB (decimal); KiB, MiB, GiB, TiB (binary).\n\
                  Examples: 37kb, 1.06GB, 64MiB. Minimum: {} bytes.\n\
                  Existing files are not overwritten. Optional u64 seed makes output reproducible.",
            minimum_size()
        );
        return Ok(());
    }
    if !(2..=3).contains(&args.len()) {
        return Err("Usage: generate_json <size> <output.json> [seed]".into());
    }
    let size = parse_size(&args[0])?;
    if size < minimum_size() {
        return Err(format!(
            "Minimum size is {} bytes to include all JSON types",
            minimum_size()
        )
        .into());
    }
    let seed = match args.get(2) {
        Some(seed) => seed.parse::<u64>()?,
        None => RandomState::new().hash_one((SystemTime::now(), std::process::id())),
    };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[1])?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let started = Instant::now();
    let count = generate(&mut writer, size, seed)?;
    writer.flush()?;
    println!(
        "Created {}: {} bytes, {} records, seed {}, {:.2?}",
        args[1],
        size,
        count,
        seed,
        started.elapsed()
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes_are_exact_and_case_insensitive() {
        for (input, expected) in [
            ("37kb", 37_000),
            ("1.06GB", 1_060_000_000),
            (" 2 KiB ", 2048),
            ("1.1KiB", 1126),
            ("500", 500),
        ] {
            assert_eq!(parse_size(input).unwrap(), expected);
        }
        for input in [
            "",
            "0",
            "-1GB",
            "1.2.3MB",
            "NaN",
            "1XB",
            "1e3",
            "999999999999999999999999999999999999999TB",
        ] {
            assert!(parse_size(input).is_err(), "{input}");
        }
    }

    #[test]
    fn output_has_exact_size_and_all_types() {
        for size in [
            minimum_size(),
            minimum_size() + 1,
            512,
            1024,
            37_000,
            1_060_000,
        ] {
            let mut bytes = Vec::new();
            let count = generate(&mut bytes, size, 42).unwrap();
            assert_eq!(bytes.len() as u64, size);
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let types = &value["types"];
            assert!(types["string"].is_string());
            assert!(types["integer"].is_number());
            assert!(types["float"].is_f64());
            assert!(types["boolean"].is_boolean());
            assert!(types["null"].is_null());
            assert!(types["array"].is_array());
            assert!(types["object"].is_object());
            assert_eq!(value["records"].as_array().unwrap().len() as u64, count);
        }
        assert!(generate(&mut Vec::new(), minimum_size() - 1, 42).is_err());
    }

    #[test]
    fn seed_reproduces_output() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        generate(&mut first, 4096, 42).unwrap();
        generate(&mut second, 4096, 42).unwrap();
        assert_eq!(first, second);
        second.clear();
        generate(&mut second, 4096, 43).unwrap();
        assert_ne!(first, second);
    }
}
