use crate::protected_data;
use anyhow::{anyhow, ensure, Result};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::io::{self, Cursor, Read, Write};

const MAGIC: &[u8] = b"ShardX workspace backup\x01";
const PURPOSE: &[u8] = b"shardx-workspace-backup-key-v1";
const CHUNK: usize = 1024 * 1024;
const TAG_SIZE: usize = 16;

fn key(bytes: &[u8]) -> Result<LessSafeKey> {
    Ok(LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, bytes)
            .map_err(|_| anyhow!("Invalid workspace backup key"))?,
    ))
}

fn nonce(index: &mut u64) -> io::Result<Nonce> {
    let mut bytes = [0; 12];
    bytes[4..].copy_from_slice(&index.to_be_bytes());
    *index = index.checked_add(1).ok_or_else(invalid)?;
    Ok(Nonce::assume_unique_for_key(bytes))
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Workspace backup failed authentication",
    )
}

pub(super) struct Encrypt<W> {
    out: W,
    key: LessSafeKey,
    index: u64,
    pending: Vec<u8>,
}

impl<W: Write> Encrypt<W> {
    pub fn new(mut out: W) -> Result<Self> {
        let mut bytes = [0; 32];
        getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("Cannot generate backup key: {e}"))?;
        let header = serde_json::to_vec(&protected_data::protect(PURPOSE, &bytes)?)?;
        out.write_all(MAGIC)?;
        out.write_all(&(header.len() as u32).to_be_bytes())?;
        out.write_all(&header)?;
        Ok(Self {
            out,
            key: key(&bytes)?,
            index: 0,
            pending: Vec::with_capacity(CHUNK + TAG_SIZE),
        })
    }

    fn frame(&mut self) -> io::Result<()> {
        let length = (self.pending.len() as u32).to_be_bytes();
        self.key
            .seal_in_place_append_tag(
                nonce(&mut self.index)?,
                Aad::from(length),
                &mut self.pending,
            )
            .map_err(|_| invalid())?;
        self.out.write_all(&length)?;
        self.out.write_all(&self.pending)?;
        self.pending.clear();
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        if !self.pending.is_empty() {
            self.frame()?;
        }
        self.frame()?; // An authenticated empty frame marks end of stream.
        self.out.flush()?;
        Ok(self.out)
    }
}

impl<W: Write> Write for Encrypt<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(CHUNK - self.pending.len());
        self.pending.extend_from_slice(&bytes[..count]);
        if self.pending.len() == CHUNK {
            self.frame()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

pub(super) struct Decrypt<R> {
    input: R,
    key: LessSafeKey,
    index: u64,
    pending: Cursor<Vec<u8>>,
    done: bool,
}

impl<R: Read> Decrypt<R> {
    pub fn new(mut input: R) -> Result<Self> {
        let mut magic = [0; MAGIC.len()];
        input.read_exact(&mut magic)?;
        ensure!(magic == MAGIC, "Not a supported ShardX workspace backup");
        let mut length = [0; 4];
        input.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        ensure!(
            (1..=16 * 1024).contains(&length),
            "Invalid workspace backup header"
        );
        let mut header = vec![0; length];
        input.read_exact(&mut header)?;
        let wrapped = serde_json::from_slice(&header)?;
        let bytes = protected_data::unprotect(PURPOSE, &wrapped)?;
        Ok(Self {
            input,
            key: key(&bytes)?,
            index: 0,
            pending: Cursor::new(Vec::new()),
            done: false,
        })
    }

    fn frame(&mut self) -> io::Result<()> {
        let mut length = [0; 4];
        self.input.read_exact(&mut length)?;
        let count = u32::from_be_bytes(length) as usize;
        if count > CHUNK {
            return Err(invalid());
        }
        let buffer = self.pending.get_mut();
        buffer.resize(count + TAG_SIZE, 0);
        self.input.read_exact(buffer)?;
        self.key
            .open_in_place(nonce(&mut self.index)?, Aad::from(length), buffer)
            .map_err(|_| invalid())?;
        buffer.truncate(count);
        self.pending.set_position(0);
        if count == 0 {
            if self.input.read(&mut [0])? != 0 {
                return Err(invalid());
            }
            self.done = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for Decrypt<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() || self.done {
            return Ok(0);
        }
        if self.pending.position() == self.pending.get_ref().len() as u64 {
            self.frame()?;
        }
        self.pending.read(bytes)
    }
}
