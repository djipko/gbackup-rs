//! # gbackup-rs - Backup your GMail
//!
//! gbackup-rs is yet another tool for backing up your GMail account with a few
//! features worth calling out:
//!
//! * It does incremental backups
//! * Uses an easy to understand config format and can handle multiple GMail accounts
//! * Backup is transactional, meaning less likelihood of corrupt backups
//! * It will pull email down in parallel which can result in a 5x + speed-up of the initial backup
//! * Written in Rust meaning more likely to be fast and secure
//!
//! ## Design
//!
//! gbackup-rs uses IMAP to sync email but does not aim to replicate all of your Gmail
//! mailbox layout. Its main goal is to back-up data, and not be a full-fledged IMAP e-mail client.
//! This may change in the future if it turns out to be useful, but for now the
//! main goal of the design is to allow for fast backups and prevent data-loss, should
//! you lose access to your Gmail account for whatever reason (after ~15 years of using GMail - I personally
//! have invaluable data from several stages of life that would be really hard to replace).
//!
//! It uses a custom schema (a sqlite db) by default to store email - and tools to
//! export to commonly used formats (such as, say, mbox) are separate (adhering to
//! good ol' unix philosophy of doing only one thing)
//!
//! ## Usage
//!
//! gbackup-rs is driven by a single configuration file. This is best explained with
//! a simple example - there is an example of a config file in the top level directory.
//! A simple and rather self-explanatory config would look something like:
//!
//! ```toml
//! [[accounts]]
//!     username = "firstname.lastname@gmail.com"
//!     [accounts.password]
//!         type = "EnvVar"
//!         name = "GMAIL_PASSWORD"
//!     [accounts.backup]
//!         type = "Sqlite"
//!         backup_dir = "/Users/firstname/gmail_backup_test/"
//!     [accounts.export]
//!         type = "Mbox"
//!         path = "/Users/firstname/gmail.mbox"
//! ```
//!
//! and by default it's expected to be stored in a file called `.gbackup.toml` in the
//! working directory (but is configurable via the -c option). Then you would simply run:
//!
//! ```bash
//!  GMAIL_PASSWORD="mysecretpass" gbackup-rs -w 10
//! ```
//!
//! This is recommended for the first back-up as it will pull down email using
//! 10 parallel IMAP connections. This results in a significant speed up! On a ~4Gb
//! mailbox - a single connection backup took ~19 minutes and 5 minutes
//! with 10 parallel connections. Since subsequent backups are incremental - running with
//! a single worker (the default) should be fine for low volume personal accounts.
//!
//! The resulting backup can then be found in the configured directory as `backup.sqlite`
//!
//! Exporting the data to a widely used format such as [mbox](http://qmail.org./man/man5/mbox.html)
//! can be done by running with the `export` subcommand
//!
//! ```bash
//! gbackup-rs -c ~/.gbackup.toml export
//! ```
//!
//! This will in turn run any export engines configured for each account in the config,
//! if any.

use chrono::{TimeZone, Utc};
use clap::{App, Arg, SubCommand};
use imap::error::Error as IMAPError;
use imap::types::{Fetch, Uid, ZeroCopy};
use imap::Session;
use mailparse;
use mailparse::MailHeaderMap;
use native_tls;
use native_tls::Error as TLSError;
use rusqlite;
use serde::Deserialize;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::iter::FromIterator;
use std::path::Path;
use std::str;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use threadpool::ThreadPool;
use toml;

type TLSStream = native_tls::TlsStream<std::net::TcpStream>;
type Rfc822Data = Vec<u8>;

const ALL_MAIL_INBOX: &str = "[Gmail]/All Mail";

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum LoadPassword {
    EnvVar { name: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BackupEngineConfig {
    Stdout { count: usize },
    Sqlite { backup_dir: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ExportEngineConfig {
    Mbox { path: Option<String> },
}

#[derive(Debug, Deserialize)]
struct GBackupAccount {
    username: String,
    password: LoadPassword,
    backup: Option<BackupEngineConfig>,
    export: Option<ExportEngineConfig>,
}

#[derive(Debug, Deserialize)]
struct GBackupConfig {
    accounts: Vec<GBackupAccount>,
}

#[derive(Debug, Error)]
enum BackupEngineError {
    #[error("Backup engine failed to load: {0}!")]
    EngineLoadError(String),
    #[error("Backup engine issue with Sqlite DB: {0}!")]
    EngineRunSqliteError(#[from] rusqlite::Error),
}

#[derive(Debug, Error)]
enum ExportEngineError {
    #[error("Export engine issue with Sqlite DB: {0}!")]
    SqliteError(#[from] rusqlite::Error),
    #[error("Export engine issue while loading data from Sqlite DB: {0}!")]
    SqliteReadError(#[from] rusqlite::types::FromSqlError),
    #[error("Operation not supported by engine")]
    UnsupportedOp,
    #[error("Export engine IO error: {0}")]
    IOErrror(#[from] io::Error),
    #[error("Error converting to Mbox fromat: {0}")]
    MboxFormatError(String),
    #[error("Export engine error parsing email: {0}")]
    MailParseError(#[from] mailparse::MailParseError),
}

#[derive(Debug, Error)]
enum GBackupError {
    #[error("TLS error {0}")]
    TLSError(#[from] TLSError),
    #[error("IMAP error {0}")]
    IMAPError(#[from] IMAPError),
    #[error("Error loading environment var {0}")]
    VarError(#[from] std::env::VarError),
    #[error("Failed to load backup config for: {0}!")]
    BackupConfigError(String),
    #[error("Error returned by the backup engine: {0}!")]
    BackupError(#[from] BackupEngineError),
    #[error("Failed to load export config for: {0}!")]
    ExportConfigError(String),
    #[error("Error returned by the export engine: {0}!")]
    ExportError(#[from] ExportEngineError),
    #[error("Error parsing rfc822 data: {0}!")]
    MailParseError(#[from] mailparse::MailParseError),
}

struct ExportEmail {
    raw_data: Rfc822Data,
    uid: Uid,
}

/// Main trait used to implement a backup strategy - see method docs for more
/// details on how this can be used, and see the 2 implemented engines
/// `StdoutEngine` and `SqliteEngine` for details
trait BackupEngine {
    /// This method should expect to be passed a list of all available UIDs
    /// (potentially for a specific mailbox, in a future implementation) and it
    /// should return a set of those that need to be backed up, potentially
    /// reporting an error.
    ///
    /// This will be used to implement incremental backups so that we don't pull
    /// down unnecessary emails
    fn filter_for_backup(
        &self,
        search_uids: &HashSet<Uid>,
    ) -> Result<HashSet<Uid>, BackupEngineError>;
    /// This method should implement the main back up logic, and will be passed
    /// emails downloaded. For more details on the Fetch struct see it's docs
    /// in the imap crate. Any progress reporting should be done from this
    /// method too - it will be called only once per each GMail account in the config
    fn run_backup(&mut self, to_backup: &Vec<&Fetch>) -> Result<(), BackupEngineError>;

    /// If an engine can be used for exporting data - this is the method to
    /// implement that will be called and pass the data to a struct implementing
    /// the `ExportEngine` trait (see bellow). `SqliteEngine` provides a good
    /// example of this
    fn get_all_emails_raw(&self) -> Result<Vec<ExportEmail>, ExportEngineError> {
        Err(ExportEngineError::UnsupportedOp)
    }
}

struct StdoutEngine<'a> {
    #[allow(dead_code)]
    account: &'a GBackupAccount,
    count: usize,
}

impl<'a> StdoutEngine<'a> {
    fn new(account: &'a GBackupAccount) -> Result<Box<StdoutEngine>, BackupEngineError> {
        if let GBackupAccount {
            backup: Some(BackupEngineConfig::Stdout { count }),
            ..
        } = account
        {
            Ok(Box::new(StdoutEngine {
                account,
                count: *count,
            }))
        } else {
            Err(BackupEngineError::EngineLoadError(format!(
                "Can't init Stdout engine from account config {:?}",
                account
            )))
        }
    }
}

impl<'a> BackupEngine for StdoutEngine<'a> {
    fn filter_for_backup(
        &self,
        search_uids: &HashSet<Uid>,
    ) -> Result<HashSet<Uid>, BackupEngineError> {
        Ok(HashSet::from_iter(
            search_uids.iter().copied().take(self.count),
        ))
    }

    fn run_backup(&mut self, to_backup: &Vec<&Fetch>) -> Result<(), BackupEngineError> {
        to_backup
            .iter()
            .map(|f| {
                println!(
                    "Would back up message w subject: {}",
                    String::from_utf8(f.envelope().unwrap().subject.unwrap().to_vec()).unwrap()
                )
            })
            .for_each(drop);
        Ok(())
    }
}

struct SqliteEngine<'a> {
    #[allow(dead_code)]
    account: &'a GBackupAccount,
    conn: rusqlite::Connection,
}

impl<'a> SqliteEngine<'a> {
    fn new(account: &'a GBackupAccount) -> Result<Box<SqliteEngine>, BackupEngineError> {
        if let GBackupAccount {
            backup: Some(BackupEngineConfig::Sqlite { backup_dir }),
            ..
        } = account
        {
            let db = Path::new(backup_dir).join("backup.sqlite");
            let conn = rusqlite::Connection::open(db)?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS email (
                uid INTEGER PRIMARY KEY,
                rfc822_body BLOB
            )",
                rusqlite::params![],
            )?;
            Ok(Box::new(SqliteEngine { account, conn }))
        } else {
            Err(BackupEngineError::EngineLoadError(format!(
                "Can't init Sqlite engine from account config {:?}",
                account
            )))
        }
    }
}

impl<'s> BackupEngine for SqliteEngine<'s> {
    fn filter_for_backup(
        &self,
        search_uids: &HashSet<Uid>,
    ) -> Result<HashSet<Uid>, BackupEngineError> {
        let mut stmt = self.conn.prepare("SELECT uid FROM email")?;
        let backed_up_uids = stmt
            .query_map(rusqlite::NO_PARAMS, |row| row.get::<_, Uid>(0))?
            .collect::<Result<HashSet<Uid>, _>>()?;
        let to_backup = search_uids.difference(&backed_up_uids).copied().collect();
        Ok(to_backup)
    }

    fn run_backup(&mut self, to_backup: &Vec<&Fetch>) -> Result<(), BackupEngineError> {
        let tx = self.conn.transaction()?;
        let mut stmt = tx.prepare("INSERT OR REPLACE INTO email VALUES (?, ?)")?;
        println!("To backup: {:?}", to_backup.len());
        let backed_up_ids = to_backup
            .iter()
            .map(|f| {
                if let Fetch { uid: Some(uid), .. } = f {
                    stmt.execute(rusqlite::params![&uid, f.body(),])
                } else {
                    Ok(0)
                }
            })
            .collect::<Result<Vec<usize>, _>>()?;
        drop(stmt);
        tx.commit()?;
        println!(
            "Backed up {}/{} emails successfully",
            backed_up_ids
                .iter()
                .filter_map(|id| if *id != 0 { Some(id) } else { None })
                .count(),
            to_backup.len()
        );
        Ok(())
    }

    fn get_all_emails_raw(&self) -> Result<Vec<ExportEmail>, ExportEngineError> {
        let mut stmt = self.conn.prepare("SELECT uid, rfc822_body FROM email")?;
        let emails = stmt
            .query_map(rusqlite::NO_PARAMS, |row| {
                let uid = row.get(0)?;
                let raw_data = row.get(1)?;
                Ok(ExportEmail { raw_data, uid })
            })?
            .collect::<Result<Vec<ExportEmail>, _>>()?;
        Ok(emails)
    }
}

fn get_backup_engine<'a>(
    account: &'a GBackupAccount,
) -> Result<Box<dyn BackupEngine + 'a>, GBackupError> {
    match account.backup {
        Some(BackupEngineConfig::Stdout { .. }) => Ok(StdoutEngine::new(account)?),
        Some(BackupEngineConfig::Sqlite { .. }) => Ok(SqliteEngine::new(account)?),
        _ => Err(GBackupError::BackupConfigError(format!(
            "Can't load engine from the empty backup config for {:?}",
            account
        ))),
    }
}

struct BackupRunner {
    account: Arc<RwLock<GBackupAccount>>,
    pool: ThreadPool,
}

impl BackupRunner {
    fn new(account: GBackupAccount, workers: usize) -> BackupRunner {
        BackupRunner {
            account: Arc::new(RwLock::new(account)),
            pool: ThreadPool::new(workers),
        }
    }

    fn new_imap_session(
        account: &Arc<RwLock<GBackupAccount>>,
    ) -> Result<Session<TLSStream>, GBackupError> {
        // TODO: Implement loading of paswords and othe auth options probably
        // to be pulled out into a single method
        let ro_acct = account.read().unwrap();
        let LoadPassword::EnvVar { name } = &ro_acct.password;
        let password = std::env::var(&name)?;

        let tls = native_tls::TlsConnector::new()?;
        let client = imap::connect(("imap.gmail.com", 993), "imap.gmail.com", &tls)?;

        let session = client.login(&ro_acct.username, password).map_err(|e| e.0)?;
        println!("Successfully created IMAP session for {}", ro_acct.username);

        Ok(session)
    }

    fn do_fetch(
        account: Arc<RwLock<GBackupAccount>>,
        chunk: &[Uid],
    ) -> Result<ZeroCopy<Vec<Fetch>>, GBackupError> {
        let mut session = BackupRunner::new_imap_session(&account)?;
        session.select(ALL_MAIL_INBOX)?;
        let fetched = session.uid_fetch(
            Vec::from_iter(chunk.iter().map(|uid| uid.to_string())).join(","),
            "BODY.PEEK[]",
        )?;
        Ok(fetched)
    }

    fn run_backup(self) -> Result<(), GBackupError> {
        let ro_acct = self.account.read().unwrap();
        let mut engine = get_backup_engine(&ro_acct)?;
        let mut session = BackupRunner::new_imap_session(&self.account)?;
        // TODO: This should also be configurable, we may want to backup all mail,
        // specific labes, etc... note also that the mailbox names may not be the same
        // depending on the language
        session.select(ALL_MAIL_INBOX)?;
        let uids = session.uid_search("ALL")?;
        println!(
            "Found {} messages in total for account {}",
            uids.len(),
            ro_acct.username
        );

        let uids_to_backup = engine.filter_for_backup(&uids)?;
        println!(
            "Uids that need backing up for account {}: {:?}",
            ro_acct.username, uids_to_backup
        );
        let chunk_len = (uids_to_backup.len() / self.pool.max_count()) + 1;
        if !uids_to_backup.is_empty() {
            println!(
                "Downloading messages for {} with {} workers",
                ro_acct.username,
                self.pool.max_count()
            );
            let (tx, rx) = channel();
            Vec::from_iter(uids_to_backup)
                .chunks(chunk_len)
                .map(|chunk| {
                    // TODO: Instead of this copy over here - we could use
                    // scoped thread from crossbeam crate
                    // (not supported by the thread pool though)
                    let curr_chunk_len = chunk.len();
                    let mut chunk_vec = Vec::with_capacity(curr_chunk_len);
                    chunk_vec.resize(curr_chunk_len, 0);
                    chunk_vec.copy_from_slice(chunk);
                    let tx = tx.clone();
                    let account = Arc::clone(&self.account);
                    self.pool.execute(move || {
                        let fetch_res = BackupRunner::do_fetch(account, &chunk_vec);
                        // This can panic but it's unlikely!
                        tx.send(fetch_res).unwrap();
                    })
                })
                .for_each(drop);
            self.pool.join();
            let emails = rx
                .try_iter()
                .collect::<Result<Vec<ZeroCopy<Vec<Fetch>>>, _>>()?;
            // Server can include so called unilateral responses - drop them if they don't
            // have a body
            let emails = emails
                .iter()
                .flatten()
                .filter(|&f| f.uid.is_some() & f.body().is_some())
                .collect();
            println!("Backing up messages for {}", ro_acct.username);
            engine.run_backup(&emails)?;
        }
        Ok(())
    }
}

/// Trait that is used to implement an export strategy. This is not necessarily
/// the same as a backup strategy as backing up and exporting email for further
/// reading/storing/processing can have different requirements. This is
/// illustrated well by the `SqliteEngine` which is a backup engine and allows
/// us to do fast, incremental, failure-proof backups using seasoned technology
/// like an sqlite DB, which we could not do with a flat file like say mbox
/// format which is very popular with email clients. We still want to be able to
/// export our email into a more widely used format, and this separation of
/// concerns is the reason for separate traits.
trait ExportEngine {
    /// Implemented this to do any of the exporting logic (including parsing)
    /// of emails, which is left to each engine, so as to be able to avoid doing
    /// unnecessary work if parsing of some parts is not needed. This method
    /// should do any I/O work, and return an appropriate error when needed.
    fn export_emails(&self, emails: &Vec<ExportEmail>) -> Result<(), ExportEngineError>;
}

struct MboxEngine<'a> {
    account: &'a GBackupAccount,
}

impl<'a> MboxEngine<'a> {
    fn get_from_line(email: &ExportEmail) -> Result<Vec<u8>, ExportEngineError> {
        let (headers, _) = mailparse::parse_headers(&email.raw_data).or(Err(
            ExportEngineError::MboxFormatError(format!(
                "Can't parse headers for email: {}!",
                email.uid
            )),
        ))?;
        let from_h = headers
            .iter()
            .filter(|h| h.get_key().to_lowercase() == "from")
            .next()
            .ok_or(ExportEngineError::MboxFormatError(format!(
                "Can't find 'From' header in: {:?}",
                email.uid
            )))?;
        let date_str =
            headers
                .get_first_value("Date")
                .ok_or(ExportEngineError::MboxFormatError(format!(
                    "Can't find 'Date' header in: {:?}",
                    email.uid
                )))?;

        let addr = mailparse::addrparse_header(&from_h)
            .or(Err(ExportEngineError::MboxFormatError(format!(
                "Can't parse address from header in email: {}!",
                email.uid
            ))))?
            .extract_single_info()
            .ok_or(ExportEngineError::MboxFormatError(format!(
                "Can't find a single address from header in email: {}",
                email.uid
            )))?
            .addr;
        let date = mailparse::dateparse(&date_str).or(Err(ExportEngineError::MboxFormatError(
            format!("Can't parse Date from header in email: {}", email.uid),
        )))?;
        let mut buffer = Vec::new();
        write!(
            &mut buffer,
            "From {} {}\n",
            addr,
            Utc.timestamp(date, 0).format("%a %b %d %T %Y")
        )?;
        Ok(buffer)
    }
}

impl ExportEngine for MboxEngine<'_> {
    fn export_emails(&self, emails: &Vec<ExportEmail>) -> Result<(), ExportEngineError> {
        let mut f: Box<dyn io::Write>;
        if let Some(ExportEngineConfig::Mbox { path: Some(path) }) = &self.account.export {
            f = Box::new(File::create(path)?);
        } else {
            f = Box::new(io::stdout());
        }
        // TODO: Make this parallel!
        emails
            .iter()
            .map(|raw_email| {
                match MboxEngine::get_from_line(raw_email) {
                    Ok(from_line) => {
                        f.write_all(&from_line)?;
                        f.write_all(&raw_email.raw_data)?;
                        f.write_all(b"\n")?;
                    }
                    Err(e) => {
                        println!("Error parsing email: {:?}!", e);
                    }
                }
                Ok(())
            })
            .collect::<Result<(), ExportEngineError>>()?;

        Ok(())
    }
}

fn get_export_engine<'a>(
    account: &'a GBackupAccount,
) -> Result<Box<dyn ExportEngine + 'a>, GBackupError> {
    match account.export {
        Some(ExportEngineConfig::Mbox { .. }) => Ok(Box::new(MboxEngine { account })),
        _ => Err(GBackupError::ExportConfigError(format!(
            "Can't load engine from the empty export config for {:?}",
            account
        ))),
    }
}

struct ExportRunner {
    account: GBackupAccount,
}

impl ExportRunner {
    fn run_export(self) -> Result<(), GBackupError> {
        let backup_engine = get_backup_engine(&self.account)?;
        let export_engine = get_export_engine(&self.account)?;

        let emails = backup_engine.get_all_emails_raw()?;
        export_engine.export_emails(&emails)?;
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn StdError>> {
    let matches = App::new("Gmail backup")
        .version("0.1")
        .author("Nikola Dipanov")
        .about("Backup your gmail stuff")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets a custom config file")
                .takes_value(true)
                .default_value(".gbackup.toml"),
        )
        .arg(
            Arg::with_name("workers")
                .short("w")
                .long("workers")
                .value_name("N_WORKERS")
                .help("Number of concurrent IMAP connections")
                .takes_value(true)
                .default_value("1"),
        )
        .subcommand(SubCommand::with_name("export").about("Run an export instead of backing up"))
        .get_matches();

    let config_file = matches.value_of("config").unwrap();
    let workers: usize = matches.value_of("workers").unwrap().parse().unwrap();
    println!("Using config file: {}", config_file);
    let config_str = fs::read_to_string(config_file)?;
    let config: GBackupConfig = toml::from_str(&config_str)?;
    println!("Loaded config from {}", config_file);
    config
        .accounts
        .into_iter()
        .map(|account| {
            if let Some(_) = matches.subcommand_matches("export") {
                let runner = ExportRunner { account };
                if let Err(error) = runner.run_export() {
                    println!("Got error running export: {}", error);
                }
            } else {
                let runner = BackupRunner::new(account, workers);
                if let Err(error) = runner.run_backup() {
                    println!("Got error running backup: {}", error);
                }
            }
        })
        .for_each(drop);
    Ok(())
}
