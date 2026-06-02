use std::{
    env::temp_dir,
    fs::File,
    io::{BufWriter, Write, copy},
    path::{Path, PathBuf},
};

use crabapple::{Authentication, Backup, backup::models::manifest::manifest_plist::ManifestData};
use imessage_database::{tables::table::DEFAULT_PATH_IOS, util::platform::Platform};

use crate::app::{
    contacts,
    error::RuntimeError,
    options::{OPTION_CLEARTEXT_PASSWORD, Options},
};

const MAX_IN_MEMORY_DECRYPT: u64 = 25 * 1024 * 1024;

/// iOS backup file IDs for the messages SQLite database and its
/// write-ahead-log sidecars. These are SHA1 hashes of the canonical
/// backup paths:
///   - main:  SHA1("HomeDomain-Library/SMS/sms.db")
///   - wal:   SHA1("HomeDomain-Library/SMS/sms.db-wal")
///   - shm:   SHA1("HomeDomain-Library/SMS/sms.db-shm")
///
/// iOS *usually* checkpoints the WAL into the main DB before backup,
/// but not always — recently-written messages can sit in the WAL when
/// the snapshot is taken. Without restoring the WAL/SHM sidecars next
/// to the main file, SQLite reads an older state and those messages
/// disappear from the export. We restore them whenever they're present
/// in the backup.
const SMS_WAL_FILE_ID: &str = "cd47480f213dba9bc38ee792775d17e3f5a73a59";
const SMS_SHM_FILE_ID: &str = "36ce215df2239f14660f11541cb64a3f790c95cd";

/// Open the iOS backup, prompting for a password if it is encrypted and one was not provided.
///
/// Returns `Ok(None)` for non-iOS platforms or for unencrypted iOS backups.
pub fn decrypt_backup(options: &Options) -> Result<Option<Backup>, RuntimeError> {
    if !matches!(options.platform, Platform::iOS) {
        return Ok(None);
    }

    // Reading Manifest.plist is cheap and tells us whether the backup is encrypted
    // without needing to derive any keys.
    let manifest_data = ManifestData::from_plist(options.db_path.join("Manifest.plist"))?;

    if !manifest_data.is_encrypted {
        if options.cleartext_password.is_some() {
            return Err(RuntimeError::InvalidOptions(format!(
                "--{OPTION_CLEARTEXT_PASSWORD} was provided, but the iOS backup at {} is not encrypted.",
                options.db_path.display()
            )));
        }
        return Ok(None);
    }

    let password = match options.cleartext_password.as_deref() {
        Some(pw) => pw.to_string(),
        None => prompt_for_password()?,
    };

    eprintln!("Decrypting iOS backup...");
    eprintln!("  [1/5] Deriving backup keys...");
    let backup = Backup::open(options.db_path.clone(), &Authentication::Password(password))?;

    Ok(Some(backup))
}

/// Prompt the user for the backup password, reading from the controlling terminal.
fn prompt_for_password() -> Result<String, RuntimeError> {
    eprintln!("Encrypted iOS backup detected. Enter password (input hidden):");
    rpassword::prompt_password("> ").map_err(|e| {
        RuntimeError::InvalidOptions(format!(
            "Unable to read password interactively ({e}); pass --{OPTION_CLEARTEXT_PASSWORD} for non-interactive use."
        ))
    })
}

/// Get the decrypted messages database from the iOS backup
///
/// The real name is `Library/SMS/sms.db`
pub fn get_decrypted_message_database(backup: &Backup) -> Result<PathBuf, RuntimeError> {
    let (_, file_id) = DEFAULT_PATH_IOS.split_at(3);
    eprintln!("  [2/5] Resolving messages database...");
    let file = backup.get_file(file_id)?;
    let mut decrypted_chat_db = backup.decrypt_entry_stream(&file)?;

    // Write decrypted sms.db into a platform-specific temporary directory
    let tmp_path = temp_dir().join("crabapple-sms.db");
    let mut file = File::create(&tmp_path)?;

    // Stream-decrypt directly into the temp file
    eprintln!("  [3/5] Decrypting messages database...");
    copy(&mut decrypted_chat_db, &mut file)?;

    // WAL / SHM sidecars: if present in the backup, decrypt them next
    // to the main db so SQLite finds them. See SMS_WAL_FILE_ID docs.
    decrypt_sidecar_if_present(backup, SMS_WAL_FILE_ID, &path_with_extra_ext(&tmp_path, "-wal"));
    decrypt_sidecar_if_present(backup, SMS_SHM_FILE_ID, &path_with_extra_ext(&tmp_path, "-shm"));

    Ok(tmp_path)
}

/// Append a suffix (e.g. `-wal` / `-shm`) to a file path. `PathBuf`
/// doesn't have a clean "append to last segment" helper.
fn path_with_extra_ext(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Decrypt a backup entry by `file_id` into `dest` when the file
/// exists; silently no-op otherwise. Used for the WAL/SHM sidecars
/// since iOS may or may not include them depending on whether the
/// WAL was clean at backup time. Logs to stderr when it does
/// extract so the user can see when WAL recovery is in play.
fn decrypt_sidecar_if_present(backup: &Backup, file_id: &str, dest: &Path) {
    let Ok(entry) = backup.get_file(file_id) else {
        return;
    };
    let Ok(mut stream) = backup.decrypt_entry_stream(&entry) else {
        eprintln!(
            "warning: WAL/SHM sidecar present in backup but decryption stream failed (file_id={file_id}). Continuing without it.",
        );
        return;
    };
    let Ok(mut out) = File::create(dest) else {
        eprintln!(
            "warning: WAL/SHM sidecar decryption succeeded but could not write to {}. Continuing without it.",
            dest.display(),
        );
        return;
    };
    if let Err(e) = copy(&mut stream, &mut out) {
        eprintln!(
            "warning: WAL/SHM sidecar copy failed: {e}. Continuing without it.",
        );
        return;
    }
    eprintln!("  → restored WAL/SHM sidecar at {}", dest.display());
}

/// Get the decrypted contacts database from the iOS backup
///
/// The real name is `Library/AddressBook/AddressBook.sqlitedb`
pub fn get_decrypted_contacts_database(backup: &Backup) -> Result<PathBuf, RuntimeError> {
    let (_, file_id) = contacts::DEFAULT_PATH_IOS.split_at(3);
    eprintln!("  [4/5] Resolving contacts database...");
    let file = backup.get_file(file_id)?;
    let mut decrypted_contacts_db = backup.decrypt_entry_stream(&file)?;

    // Write decrypted contacts.db into a platform-specific temporary directory
    let tmp_path = temp_dir().join("crabapple-contacts.db");
    let mut file = File::create(&tmp_path)?;

    // Stream-decrypt directly into the temp file
    eprintln!("  [5/5] Decrypting contacts database...");
    copy(&mut decrypted_contacts_db, &mut file)?;

    Ok(tmp_path)
}

/// For an *unencrypted* iOS backup, stage the messages DB + WAL + SHM
/// sidecars into a temp directory so SQLite finds them together.
///
/// Without this the unencrypted path read directly from
/// `<backup>/3d/3d0d7e5f...` and SQLite looked for
/// `<backup>/3d/3d0d7e5f...-wal` (doesn't exist; WAL sits at a
/// different SHA1 hash). Recent messages that hadn't been
/// checkpointed at backup time were silently invisible. See the
/// `SMS_WAL_FILE_ID` docs for the full explanation.
///
/// Returns the path to the staged main DB. If WAL/SHM aren't in the
/// backup (clean checkpoint) the main DB still loads correctly.
pub fn stage_unencrypted_ios_db(backup_root: &Path) -> Result<PathBuf, RuntimeError> {
    let main_src = backup_root.join(DEFAULT_PATH_IOS);
    let staged = temp_dir().join("crabapple-sms.db");
    std::fs::copy(&main_src, &staged)?;
    copy_unencrypted_sidecar_if_present(
        backup_root,
        SMS_WAL_FILE_ID,
        &path_with_extra_ext(&staged, "-wal"),
    );
    copy_unencrypted_sidecar_if_present(
        backup_root,
        SMS_SHM_FILE_ID,
        &path_with_extra_ext(&staged, "-shm"),
    );
    Ok(staged)
}

fn copy_unencrypted_sidecar_if_present(backup_root: &Path, file_id: &str, dest: &Path) {
    // Backup file IDs are stored under `<2-char-prefix>/<full-hash>`
    let src = backup_root.join(&file_id[..2]).join(file_id);
    if !src.exists() {
        return;
    }
    if let Err(e) = std::fs::copy(&src, dest) {
        eprintln!(
            "warning: failed to stage WAL/SHM sidecar from {} to {}: {e}. Continuing without it.",
            src.display(),
            dest.display(),
        );
        return;
    }
    eprintln!("  → restored WAL/SHM sidecar at {}", dest.display());
}

/// Decrypt a file from the iOS backup
pub fn decrypt_file(backup: &Backup, from: &Path) -> Result<PathBuf, RuntimeError> {
    match backup.get_file(
        from.file_name()
            .ok_or(RuntimeError::FileNameError)?
            .to_str()
            .ok_or(RuntimeError::FileNameError)?,
    ) {
        Ok(file) => {
            let temp_dir = temp_dir().join(&file.file_id);
            let mut temp_file = File::create(&temp_dir)?;

            // Get the size of the file
            let file_size = file.metadata.size;
            // If the file is larger than 25MB, we will stream decryption from/to disk
            // otherwise, we will decrypt in memory
            if file_size > MAX_IN_MEMORY_DECRYPT {
                // Copy via disk
                let mut decryption_stream = backup.decrypt_entry_stream(&file)?;
                let mut writer = BufWriter::new(temp_file);

                // Copy all data from reader to writer
                copy(&mut decryption_stream, &mut writer)?;

                // Ensure all buffered data is flushed to disk
                writer.flush()?;
            } else {
                // Copy via memory
                let decrypted_bytes = backup.decrypt_entry(&file)?;
                temp_file.write_all(&decrypted_bytes)?;
            }

            // Ensure we remove the temporary file later
            Ok(temp_dir)
        }
        Err(why) => Err(RuntimeError::BackupError(why)),
    }
}
