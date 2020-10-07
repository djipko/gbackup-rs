# gbackup-rs - Backup your GMail

[![Build Status](https://travis-ci.org/djipko/gbackup-rs.svg?branch=master)](https://travis-ci.org/djipko/gbackup-rs)

gbackup-rs is yet another tool for backing up your GMail account with a few
features worth calling out:

* It does incremental backups
* Uses an easy to understand config format and can handle multiple GMail accounts
* Backup is transactional, meaning less likelihood of corrupt backups
* It will pull email down in parallel which can result in a 5x + speed-up of the initial backup
* Written in Rust meaning more likely to be fast and secure

It uses a custom schema (a sqlite db) by default to store email - and tools to
export to commonly used formats (such as, say, mbox) are separate (adhering to
good ol' unix philosophy of doing only one thing)

## Design

gbackup-rs uses IMAP to sync email but does not aim to replicate all of your Gmail
mailbox layout. Its main goal is to back data up, and not be a full-fledged IMAP e-mail client.
This may change in the future if it turns out to be useful, but for now the
main goal of the design is to allow for fast backups and prevent data-loss, should
you lose access to your Gmail account for whatever reason (after ~15 years of using GMail - I personally
have invaluable data from several stages of life that would be really hard to replace).

## Usage

There is an example of a config file in the top level directory. A simple and
rather self-explanatory config would look something like:

```toml
[[accounts]]
    username = "firstname.lastname@gmail.com"
    [accounts.password]
        type = "EnvVar"
        name = "GMAIL_PASSWORD"
    [accounts.backup]
        type = "Sqlite"
        backup_dir = "/Users/firstname/gmail_backup_test/"
    [accounts.export]
        type = "Mbox"
        path = "/Users/firstname/gmail.mbox"
```

and by default it's expected to be stored in a file called `.gbackup.toml` in the
working directory (but is configurable via the -c option). Then you would simply run:

```bash
 GMAIL_PASSWORD="mysecretpass" ./gbackup-rs -w 10
```

This is recommended for the first back-up as it will pull down email using
10 parallel IMAP connections. This results in a significant speed up! On a ~4Gb
mailbox - a single connection backup took ~19 minutes and 5 minutes
with 10 parallel connections. Since subsequent backups are incremental - running with
a single worker (the default) should be fine for low volume personal accounts.

The resulting backup can then be found in the configured directory as `backup.sqlite`

NOTE that you will need to set up an app password for you gmail account
 [here](https://myaccount.google.com/apppasswords)

Exporting the data to a widely used format such as [mbox](http://qmail.org./man/man5/mbox.html)
can be done by running with the `export` subcommand

```bash
./gbackup-rs -c ~/.gbackup.toml export
```

This will in turn run any export engines configured for each account in the config,
if any.

## Future features

gbackup-rs is highly experimental at this point and many things are likely to 
change (hopefully for the better).
Check out the issues tracker to see some of the stuff that's been worked on.
