use std::{fs::remove_file, path::Path};

use crabapple::Backup;
use imessage_database::{tables::table::get_connection, util::platform::Platform};
use rusqlite::Connection;

use crate::app::{
    compatibility::backup::{
        decrypt_backup, get_decrypted_contacts_database, get_decrypted_message_database,
        stage_unencrypted_ios_db,
    },
    contacts::{ContactsIndex, DEFAULT_PATH_IOS},
    error::RuntimeError,
    options::Options,
};

pub struct DataSource {
    /// The connection we use to query the database
    ///
    /// This is wrapped in `Option` to allow for taking/dropping it when cleaning up temporary files,
    /// but should always be `Some` during normal operation.
    messages_connection: Option<Connection>,
    /// Index of contacts, mapping emails and phone numbers to name data
    ///
    /// If construction fails, this will be an empty index, and a warning will be logged.
    pub contacts_index: ContactsIndex,
    /// An optional encrypted iOS backup
    pub backup: Option<Backup>,
}

impl DataSource {
    /// Create a new `DataSource` from the provided Options
    ///
    /// Options constructor determines the platform and database location logic already,
    /// so this just uses that to create the appropriate connections and indexes.
    pub fn from(options: &Options) -> Result<Self, RuntimeError> {
        match options.platform {
            Platform::macOS => {
                let messages_path = options.get_db_path();

                let contacts_index =
                    Self::get_contacts_index(options.contacts_path.as_deref()).unwrap_or_default();

                Ok(Self {
                    messages_connection: Some(get_connection(&messages_path)?),
                    contacts_index,
                    backup: None,
                })
            }
            Platform::iOS => match decrypt_backup(options)? {
                Some(backup) => {
                    let messages_path = get_decrypted_message_database(&backup)?;
                    let contacts_path = get_decrypted_contacts_database(&backup)?;

                    eprintln!(
                        "Decrypted iOS backup: {} (version {})\n",
                        backup.lockdown().device_name,
                        backup.lockdown().product_version,
                    );

                    let contacts_index =
                        Self::get_contacts_index(Some(&contacts_path)).unwrap_or_default();

                    // Clean up decrypted contacts database file
                    if let Err(e) = remove_file(&contacts_path) {
                        eprintln!(
                            "warning: failed to remove temporary contacts database at {}: {e}",
                            contacts_path.display()
                        );
                    }

                    Ok(Self {
                        messages_connection: Some(get_connection(&messages_path)?),
                        contacts_index,
                        backup: Some(backup),
                    })
                }
                None => {
                    // Unencrypted iOS backup: copy main DB + WAL + SHM
                    // sidecars to a temp dir so SQLite finds them under
                    // the conventional `.db` / `.db-wal` / `.db-shm`
                    // names. Reading `<backup>/3d/3d0d7e5f...` directly
                    // misses the WAL (which lives at a different file id)
                    // and silently drops any messages iOS hadn't
                    // checkpointed at backup time.
                    let messages_path = stage_unencrypted_ios_db(&options.db_path)?;
                    let contacts_index =
                        Self::get_contacts_index(Some(&options.db_path.join(DEFAULT_PATH_IOS)))
                            .unwrap_or_default();

                    Ok(Self {
                        messages_connection: Some(get_connection(&messages_path)?),
                        contacts_index,
                        backup: None,
                    })
                }
            },
        }
    }

    /// Construct a `ContactsIndex`, if possible, logging a warning if the construction fails
    fn get_contacts_index(path: Option<&Path>) -> Option<ContactsIndex> {
        match ContactsIndex::build(path) {
            Ok(index) => Some(index),
            Err(e) => {
                eprintln!(
                    "Unable to build contacts index: {e}\nContinuing without contact names..."
                );
                None
            }
        }
    }

    /// Get the current database connection, if it is alive
    ///
    /// # Panics
    ///
    /// Panics if the database connection is closed.
    pub(crate) fn db(&self) -> &Connection {
        match self.messages_connection.as_ref() {
            Some(db) => db,
            None => {
                panic!("Database connection is closed!");
            }
        }
    }
}

// MARK: Drop
impl Drop for DataSource {
    fn drop(&mut self) {
        if let Some(backup) = &self.backup {
            // Remove the temporary `sms.db` file if it was created
            if backup.manifest_db.is_temporary
                && let Some(conn) = self.messages_connection.take()
            {
                let path = conn.path().map(|p| p.to_string());
                conn.close().ok();

                // Remove the file, ignoring errors if any. Skip silently if the
                // connection had no filesystem path (e.g. opened in-memory),
                // which shouldn't be reachable after a successful decrypt.
                if let Some(path) = path
                    && let Err(e) = remove_file(&path)
                {
                    eprintln!(
                        "warning: failed to remove temporary messages database at {path}: {e}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::app::{export_type::ExportType, options::Options};
    use imessage_database::util::platform::Platform;

    #[test]
    fn test_data_source_from_macos() {
        // Create fake options for macOS
        let mut options = Options::fake_options(ExportType::Txt);
        options.platform = Platform::macOS;

        // Test that DataSource can be created for macOS
        let result = DataSource::from(&options);
        assert!(result.is_ok());

        let ds = result.unwrap();
        assert!(ds.messages_connection.is_some());
        assert!(ds.backup.is_none());
    }

    #[test]
    fn test_data_source_db() {
        let mut options = Options::fake_options(ExportType::Txt);
        options.platform = Platform::macOS;
        let ds = DataSource::from(&options).unwrap();

        // Test that `db()` returns a connection
        let conn = ds.db();
        assert!(conn.path().is_some());
    }

    #[test]
    fn test_data_source_invalid_db_path() {
        let mut options = Options::fake_options(ExportType::Txt);
        options.platform = Platform::macOS;
        options.db_path = PathBuf::from("/nonexistent/path.db");

        // Test that creation fails with invalid path
        let result = DataSource::from(&options);
        assert!(result.is_err());
    }
}
