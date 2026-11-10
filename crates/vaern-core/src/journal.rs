//! Append-only, hash-chained effect journal: the file every run writes and
//! every later feature (budgets, replay) reads.
//!
//! One record per line, framed as `{"h":"<sha-256 hex>","r":<record>}`. The
//! hash of record `n` is SHA-256 over the previous record's hash (64 lowercase
//! hex digits, 64 zeros for the first), a newline, and the exact bytes of
//! `<record>`. Because the record always starts at byte 76 and ends one byte
//! before the line end, a verifier hashes the bytes that were written; nothing
//! is re-serialised, so key order and number formatting cannot break a chain.
//!
//! The format is deliberately boring: JSON Lines readable by `jq .r`, integer
//! fields only (no floats), every line written and flushed as it is appended.
//! The chain detects any edit, reordering, insertion or deletion of records.
//! A cut-off tail looks like a crash: the run is reported unsealed, not
//! tampered, because the two cannot be told apart from the file alone.

use crate::error::{Result, VaernError};
use serde_json::{Map, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The chain value before the first record.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Bytes before the record in every line: `{"h":"`, 64 hex digits, `","r":`.
const RECORD_OFFSET: usize = 76;

/// An open journal for one run.
pub struct Journal {
    file: std::fs::File,
    path: PathBuf,
    run_id: String,
    next_seq: u64,
    prev: String,
    /// Running SHA-256 of the transcript; see [`Journal::console`].
    transcript: ring::digest::Context,
    sealed: bool,
}

/// The outcome of [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Records whose hashes verified.
    pub records: u64,
    /// Hash of the last verified record, or [`GENESIS`] for an empty file.
    pub final_hash: String,
    /// The last record is `run.end`.
    pub sealed: bool,
    /// The file ends in a partial line, as a crash during a write leaves it.
    pub torn_tail: bool,
}

impl Journal {
    /// Creates `<home>/runs/<run-id>/journal.jsonl` for a new run and points
    /// `<home>/runs/latest` at it.
    pub fn create(home: &Path) -> Result<Journal> {
        let runs = home.join("runs");
        std::fs::create_dir_all(&runs).map_err(|err| create_error(&runs, &err))?;
        let base = run_id_now();
        // Two runs started in the same second by processes whose ids collide
        // in the low bits get the next suffix.
        let mut attempt = 0u32;
        let (run_id, dir) = loop {
            let run_id = if attempt == 0 {
                base.clone()
            } else {
                format!("{base}{attempt}")
            };
            let dir = runs.join(&run_id);
            match std::fs::create_dir(&dir) {
                Ok(()) => break (run_id, dir),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => {
                    attempt += 1;
                }
                Err(err) => return Err(create_error(&dir, &err)),
            }
        };
        let path = dir.join("journal.jsonl");
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| create_error(&path, &err))?;

        // The temporary file carries the run id: runs started together in
        // one home would otherwise share it, and the second rename would
        // find it already moved.
        let latest_tmp = runs.join(format!("latest.{run_id}.tmp"));
        if let Err(err) = std::fs::write(&latest_tmp, &run_id)
            .and_then(|()| std::fs::rename(&latest_tmp, runs.join("latest")))
        {
            // Best effort: the run is refused either way, and a leftover
            // temporary file is harmless.
            let _ = std::fs::remove_file(&latest_tmp);
            return Err(create_error(&runs.join("latest"), &err));
        }

        Ok(Journal {
            file,
            path,
            run_id,
            next_seq: 0,
            prev: GENESIS.to_string(),
            transcript: ring::digest::Context::new(&ring::digest::SHA256),
            sealed: false,
        })
    }

    /// Opens an existing chained file to append to it, creating it when it
    /// does not exist. The chain is verified first and continues from its
    /// last record. A partial final line is cut off: the write that left it
    /// never returned, so nothing that depended on it was done.
    pub(crate) fn resume(path: &Path) -> Result<Journal> {
        // An append-only handle cannot truncate on Windows, so the torn line
        // is cut through a write handle before the append handle is opened.
        let writer = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|err| create_error(path, &err))?;
        let verified = verify(path)?;
        if verified.torn_tail {
            let bytes = std::fs::read(path).map_err(|err| create_error(path, &err))?;
            let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            writer
                .set_len(keep as u64)
                .and_then(|()| writer.sync_data())
                .map_err(|err| write_error(path, &err.to_string()))?;
        }
        drop(writer);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|err| create_error(path, &err))?;
        Ok(Journal {
            file,
            path: path.to_path_buf(),
            run_id: String::new(),
            next_seq: verified.records,
            prev: verified.final_hash,
            transcript: ring::digest::Context::new(&ring::digest::SHA256),
            sealed: false,
        })
    }

    /// The id `vaern replay` and `vaern journal verify` accept.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Path of the journal file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one record and returns its sequence number. `fields` follow
    /// `seq`, `kind` and `at_us` in the order given.
    pub fn append(&mut self, kind: &str, fields: Map<String, Value>) -> Result<u64> {
        self.write_record(kind, fields, false)
    }

    /// Like [`Journal::append`], and the record is on disk before this
    /// returns. Used where a later effect relies on the record surviving a
    /// crash, such as a budget reservation made before its request is sent.
    pub fn append_durable(&mut self, kind: &str, fields: Map<String, Value>) -> Result<u64> {
        self.write_record(kind, fields, true)
    }

    /// Records text the script wrote to stdout (`fd` 1) or stderr (`fd` 2),
    /// exactly as written, newline included. These records are the run's
    /// transcript; its hash covers `<fd>:<byte length>:<text>` per record.
    pub fn console(&mut self, fd: u8, text: &str) -> Result<u64> {
        let mut fields = Map::new();
        fields.insert("fd".to_string(), Value::from(fd));
        fields.insert("text".to_string(), Value::from(text));
        let seq = self.append("console", fields)?;
        self.transcript
            .update(format!("{fd}:{}:", text.len()).as_bytes());
        self.transcript.update(text.as_bytes());
        Ok(seq)
    }

    /// Appends `run.end` and syncs the file. Later calls do nothing, so the
    /// first outcome, such as `process.exit`, is the one recorded.
    pub fn seal(&mut self, exit_code: i32, error: Option<&str>) -> Result<()> {
        if self.sealed {
            return Ok(());
        }
        self.sealed = true;
        let mut fields = Map::new();
        fields.insert("exit_code".to_string(), Value::from(exit_code));
        if let Some(error) = error {
            fields.insert("error".to_string(), Value::from(error));
        }
        fields.insert("records".to_string(), Value::from(self.next_seq + 1));
        fields.insert(
            "transcript_sha256".to_string(),
            Value::from(hex(self.transcript.clone().finish().as_ref())),
        );
        self.append_durable("run.end", fields)?;
        Ok(())
    }

    fn write_record(
        &mut self,
        kind: &str,
        fields: Map<String, Value>,
        durable: bool,
    ) -> Result<u64> {
        let seq = self.next_seq;
        let mut record = Map::new();
        record.insert("seq".to_string(), Value::from(seq));
        record.insert("kind".to_string(), Value::from(kind));
        record.insert("at_us".to_string(), Value::from(now_us()));
        record.extend(fields);
        let body = serde_json::to_string(&Value::Object(record)).map_err(|err| {
            write_error(&self.path, &format!("the record cannot be encoded: {err}"))
        })?;
        let hash = chain_hash(&self.prev, body.as_bytes());
        let line = format!("{{\"h\":\"{hash}\",\"r\":{body}}}\n");
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
            .and_then(|()| {
                if durable {
                    self.file.sync_data()
                } else {
                    Ok(())
                }
            })
            .map_err(|err| write_error(&self.path, &err.to_string()))?;
        self.prev = hash;
        self.next_seq += 1;
        Ok(seq)
    }
}

/// SHA-256 over `prev ‖ "\n" ‖ record`, as lowercase hex.
pub fn chain_hash(prev: &str, record: &[u8]) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(prev.as_bytes());
    ctx.update(b"\n");
    ctx.update(record);
    hex(ctx.finish().as_ref())
}

/// SHA-256 of `bytes` as lowercase hex, the form journal records store.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// Checks every record's hash and sequence number.
pub fn verify(path: &Path) -> Result<Verified> {
    let file = std::fs::File::open(path).map_err(|err| VaernError::Journal {
        stage: "read".to_string(),
        path: path.display().to_string(),
        reason: err.to_string(),
        action: "Check the run id; `vaern journal verify latest` checks the newest run."
            .to_string(),
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut prev = GENESIS.to_string();
    let mut records = 0u64;
    let mut last_kind = String::new();
    let mut torn_tail = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|err| VaernError::Journal {
                stage: "read".to_string(),
                path: path.display().to_string(),
                reason: err.to_string(),
                action: "Check the disk and retry.".to_string(),
            })?;
        if read == 0 {
            break;
        }
        let complete = line.last() == Some(&b'\n');
        if !complete {
            // Only the final line can lack its newline; a crash mid-write
            // leaves it so. It is not counted, and earlier records stand.
            torn_tail = true;
            break;
        }
        let tampered = |reason: &str| {
            VaernError::JournalTampered {
            path: path.display().to_string(),
            seq: records,
            reason: reason.to_string(),
            action: "Do not trust this run; restore the journal from a copy whose final hash you recorded."
                .to_string(),
        }
        };
        let text = &line[..line.len() - 1];
        if last_kind == "run.end" {
            return Err(tampered("a record follows run.end"));
        }
        if text.len() < RECORD_OFFSET + 2
            || !text.starts_with(b"{\"h\":\"")
            || &text[70..RECORD_OFFSET] != b"\",\"r\":"
            || text.last() != Some(&b'}')
        {
            return Err(tampered("the line is not a framed journal record"));
        }
        let stored = &text[6..70];
        let body = &text[RECORD_OFFSET..text.len() - 1];
        let computed = chain_hash(&prev, body);
        if stored != computed.as_bytes() {
            return Err(tampered(
                "the hash does not match the record and the chain before it",
            ));
        }
        let record: Value =
            serde_json::from_slice(body).map_err(|_| tampered("the record is not valid JSON"))?;
        if record.get("seq").and_then(Value::as_u64) != Some(records) {
            return Err(tampered("the sequence number is out of order"));
        }
        last_kind = record
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        prev = computed;
        records += 1;
    }
    Ok(Verified {
        records,
        final_hash: prev,
        sealed: last_kind == "run.end",
        torn_tail,
    })
}

/// The journal file for `id` (or `latest`) under `home`.
pub fn locate(home: &Path, id: &str) -> Result<PathBuf> {
    let runs = home.join("runs");
    let locate_error = |reason: String| VaernError::Journal {
        stage: "locate".to_string(),
        path: runs.display().to_string(),
        reason,
        action: "Pass a run id from .vaern/runs, or `latest`.".to_string(),
    };
    let id = if id == "latest" {
        std::fs::read_to_string(runs.join("latest"))
            .map_err(|err| locate_error(format!("no latest run is recorded: {err}")))?
            .trim()
            .to_string()
    } else {
        id.to_string()
    };
    // Ids are generated as digits, hex and dashes; anything else could name
    // a path outside the runs directory.
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(locate_error(format!("'{id}' is not a run id")));
    }
    let path = runs.join(&id).join("journal.jsonl");
    if !path.is_file() {
        return Err(locate_error(format!("run '{id}' has no journal")));
    }
    Ok(path)
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `YYYYMMDD-HHMMSS-xxxxxx`: the UTC start time, then six hex digits from the
/// process id and the clock's sub-second part.
pub(crate) fn run_id_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let (year, month, day) = civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0));
    let rem = secs % 86_400;
    let tag = (std::process::id() ^ now.subsec_nanos()) & 0x00ff_ffff;
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}-{tag:06x}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Gregorian date for days since 1970-01-01 (H. Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn create_error(path: &Path, err: &std::io::Error) -> VaernError {
    VaernError::Journal {
        stage: "create".to_string(),
        path: path.display().to_string(),
        reason: err.to_string(),
        action: "Make the directory writable, set VAERN_HOME to a writable directory, or run with --no-journal."
            .to_string(),
    }
}

fn write_error(path: &Path, reason: &str) -> VaernError {
    VaernError::Journal {
        stage: "write".to_string(),
        path: path.display().to_string(),
        reason: reason.to_string(),
        action: "Free disk space or fix the journal directory; the effect was not performed."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn sealed_run(home: &Path) -> Journal {
        let mut journal = Journal::create(home).unwrap();
        journal
            .append("run.start", fields(&[("script", Value::from("main.ts"))]))
            .unwrap();
        journal
            .append(
                "console",
                fields(&[("fd", Value::from(1)), ("text", Value::from("hi"))]),
            )
            .unwrap();
        journal
            .append(
                "console",
                fields(&[("fd", Value::from(2)), ("text", Value::from("ho"))]),
            )
            .unwrap();
        journal
            .append_durable("run.end", fields(&[("exit_code", Value::from(0))]))
            .unwrap();
        journal
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }

    fn tampered_at(path: &Path) -> u64 {
        match verify(path) {
            Err(VaernError::JournalTampered { seq, .. }) => seq,
            other => panic!("expected tampering to be detected, got {other:?}"),
        }
    }

    #[test]
    fn test_sealed_run_verifies_with_the_documented_hash() {
        let home = tempfile::tempdir().unwrap();
        let journal = sealed_run(home.path());
        let verified = verify(journal.path()).unwrap();
        assert_eq!(verified.records, 4);
        assert!(verified.sealed);
        assert!(!verified.torn_tail);

        // Recompute record 0 from the documented construction alone.
        let first = &lines(journal.path())[0];
        let body = &first[RECORD_OFFSET..first.len() - 1];
        let mut input = format!("{GENESIS}\n").into_bytes();
        input.extend_from_slice(body.as_bytes());
        assert_eq!(&first[6..70], sha256_hex(&input));
        assert!(
            body.starts_with(r#"{"seq":0,"kind":"run.start","at_us":"#),
            "{body}"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("runs/latest")).unwrap(),
            journal.run_id()
        );
        assert_eq!(locate(home.path(), "latest").unwrap(), journal.path());
    }

    #[test]
    fn test_runs_started_together_in_one_home_all_get_a_journal() {
        let home = tempfile::tempdir().unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (home, start) = (home.path().to_path_buf(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    (0..25)
                        .map(|_| Journal::create(&home).map(|journal| journal.run_id().to_string()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut ids = Vec::new();
        for worker in workers {
            for created in worker.join().unwrap() {
                ids.push(created.unwrap());
            }
        }
        assert_eq!(ids.len(), 200);
        // The pointer names one of the runs, and no temporary file is left.
        let latest = std::fs::read_to_string(home.path().join("runs/latest")).unwrap();
        assert!(ids.contains(&latest), "{latest}");
        let left: Vec<_> = std::fs::read_dir(home.path().join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn test_edits_reorders_and_deletions_are_detected() {
        let home = tempfile::tempdir().unwrap();
        let journal = sealed_run(home.path());
        let path = journal.path().to_path_buf();
        let original = lines(&path);

        // One changed byte in a record body.
        let mut edited = original.clone();
        edited[2] = edited[2].replace("\"ho\"", "\"hO\"");
        write_lines(&path, &edited);
        assert_eq!(tampered_at(&path), 2);

        // Two records swapped.
        let mut swapped = original.clone();
        swapped.swap(1, 2);
        write_lines(&path, &swapped);
        assert_eq!(tampered_at(&path), 1);

        // A record removed from the middle.
        let mut removed = original.clone();
        removed.remove(1);
        write_lines(&path, &removed);
        assert_eq!(tampered_at(&path), 1);

        // A record appended after the seal, with a valid hash.
        let mut appended = original.clone();
        let body = r#"{"seq":4,"kind":"console","at_us":0}"#;
        let last_hash = &original[3][6..70];
        appended.push(format!(
            "{{\"h\":\"{}\",\"r\":{body}}}",
            chain_hash(last_hash, body.as_bytes())
        ));
        write_lines(&path, &appended);
        assert_eq!(tampered_at(&path), 4);
    }

    #[test]
    fn test_cut_off_tail_reads_as_unsealed_not_tampered() {
        let home = tempfile::tempdir().unwrap();
        let journal = sealed_run(home.path());
        let path = journal.path().to_path_buf();
        let original = lines(&path);

        write_lines(&path, &original[..2]);
        let verified = verify(&path).unwrap();
        assert_eq!((verified.records, verified.sealed), (2, false));

        // A half-written last line, as a crash leaves it.
        let mut text = original[..3].join("\n");
        text.push('\n');
        text.push_str(&original[3][..40]);
        std::fs::write(&path, text).unwrap();
        let verified = verify(&path).unwrap();
        assert_eq!(
            (verified.records, verified.sealed, verified.torn_tail),
            (3, false, true)
        );
    }

    #[test]
    fn test_run_ids_are_plain_and_paths_cannot_escape() {
        let home = tempfile::tempdir().unwrap();
        let a = Journal::create(home.path()).unwrap();
        let b = Journal::create(home.path()).unwrap();
        assert_ne!(a.run_id(), b.run_id());
        let id = a.run_id();
        assert_eq!(id.len(), 22, "{id}");
        assert!(
            id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
            "{id}"
        );
        for bad in ["../x", "..", "a/b", ""] {
            assert!(locate(home.path(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_402), (2025, 11, 10));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }
}
