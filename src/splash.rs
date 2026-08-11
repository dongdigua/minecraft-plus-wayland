use std::error::Error;

use rand::{Rng, seq::SliceRandom};

const START_MARKER: &[u8] = b"Full of stars!\n";
const FALLBACK_SPLASH: &str = "Splashes not included";
const EXPECTED_SPLASH_COUNT: usize = 142;

pub(crate) fn pick_splash() -> Result<String, Box<dyn Error>> {
    let wasm = crate::resources::load_web_wasm()?;
    Ok(pick_from_wasm(&wasm, &mut rand::thread_rng())?.to_owned())
}

fn pick_from_wasm<'a, R>(wasm: &'a [u8], random: &mut R) -> Result<&'a str, Box<dyn Error>>
where
    R: Rng + ?Sized,
{
    let table = extract_splash_table(wasm)?;
    Ok(table
        .lines()
        .collect::<Vec<_>>()
        .choose(random)
        .copied()
        .unwrap_or(FALLBACK_SPLASH))
}

fn extract_splash_table(wasm: &[u8]) -> Result<&str, Box<dyn Error>> {
    let mut starts = wasm
        .windows(START_MARKER.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == START_MARKER).then_some(offset));
    let start = starts
        .next()
        .ok_or("Web WASM does not contain the splash-table start marker")?;
    if starts.next().is_some() {
        return Err("Web WASM contains more than one splash-table start marker".into());
    }

    let fallback_search_start = start + START_MARKER.len();
    let fallback_marker = FALLBACK_SPLASH.as_bytes();
    let fallback_offset = wasm[fallback_search_start..]
        .windows(fallback_marker.len())
        .position(|window| window == fallback_marker)
        .ok_or("Web WASM does not contain the splash fallback after the table")?;
    let end = fallback_search_start + fallback_offset;
    let table = std::str::from_utf8(&wasm[start..end])?;
    let splashes = table.lines().collect::<Vec<_>>();

    if !table.ends_with('\n') {
        return Err("Web WASM splash table does not end with a newline".into());
    }
    if splashes.len() != EXPECTED_SPLASH_COUNT {
        return Err(format!(
            "Web WASM splash table contains {} entries instead of {EXPECTED_SPLASH_COUNT}",
            splashes.len()
        )
        .into());
    }
    if splashes.iter().any(|splash| splash.is_empty()) {
        return Err("Web WASM splash table contains an empty entry".into());
    }

    Ok(table)
}

#[cfg(test)]
mod tests {
    use rand::rngs::mock::StepRng;

    use super::*;

    fn test_wasm(splash_count: usize) -> Vec<u8> {
        let mut wasm = b"\0asm-test-prefix".to_vec();
        wasm.extend_from_slice(START_MARKER);
        for index in 1..splash_count {
            wasm.extend_from_slice(format!("Test splash {index}!\n").as_bytes());
        }
        wasm.extend_from_slice(FALLBACK_SPLASH.as_bytes());
        wasm.extend_from_slice(b"-test-suffix");
        wasm
    }

    #[test]
    fn extracts_the_validated_table_from_wasm_bytes() {
        let wasm = test_wasm(EXPECTED_SPLASH_COUNT);
        let table = extract_splash_table(&wasm).unwrap();

        assert!(table.starts_with("Full of stars!\n"));
        assert!(table.ends_with("Test splash 141!\n"));
        assert_eq!(table.lines().count(), EXPECTED_SPLASH_COUNT);
    }

    #[test]
    fn selection_returns_an_extracted_splash() {
        let wasm = test_wasm(EXPECTED_SPLASH_COUNT);
        let mut random = StepRng::new(42, 17);
        let splash = pick_from_wasm(&wasm, &mut random).unwrap();

        assert!(
            extract_splash_table(&wasm)
                .unwrap()
                .lines()
                .any(|candidate| candidate == splash)
        );
    }

    #[test]
    fn rejects_missing_ambiguous_or_wrong_sized_tables() {
        assert!(extract_splash_table(b"\0asm").is_err());

        let mut duplicate = test_wasm(EXPECTED_SPLASH_COUNT);
        duplicate.extend_from_slice(START_MARKER);
        assert!(extract_splash_table(&duplicate).is_err());

        assert!(extract_splash_table(&test_wasm(EXPECTED_SPLASH_COUNT - 1)).is_err());
    }
}
