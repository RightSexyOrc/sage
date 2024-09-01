use chia::{bls::PublicKey, protocol::Bytes32};
use sqlx::SqliteExecutor;

use crate::{to_bytes, to_bytes32, Database, DatabaseTx, Result};

impl Database {
    pub async fn insert_derivation(
        &self,
        p2_puzzle_hash: Bytes32,
        index: u32,
        hardened: bool,
        synthetic_key: PublicKey,
    ) -> Result<()> {
        insert_derivation(&self.pool, p2_puzzle_hash, index, hardened, synthetic_key).await
    }

    pub async fn derivation_index(&self, hardened: bool) -> Result<u32> {
        derivation_index(&self.pool, hardened).await
    }

    pub async fn max_used_derivation_index(&self) -> Result<Option<u32>> {
        max_used_derivation_index(&self.pool).await
    }

    pub async fn p2_puzzle_hashes(&self) -> Result<Vec<Bytes32>> {
        p2_puzzle_hashes(&self.pool).await
    }

    pub async fn synthetic_key(&self, p2_puzzle_hash: Bytes32) -> Result<PublicKey> {
        synthetic_key(&self.pool, p2_puzzle_hash).await
    }

    pub async fn is_p2_puzzle_hash(&self, p2_puzzle_hash: Bytes32) -> Result<bool> {
        is_p2_puzzle_hash(&self.pool, p2_puzzle_hash).await
    }
}

impl<'a> DatabaseTx<'a> {
    pub async fn insert_derivation(
        &mut self,
        p2_puzzle_hash: Bytes32,
        index: u32,
        hardened: bool,
        synthetic_key: PublicKey,
    ) -> Result<()> {
        insert_derivation(
            &mut *self.tx,
            p2_puzzle_hash,
            index,
            hardened,
            synthetic_key,
        )
        .await
    }

    pub async fn derivation_index(&mut self, hardened: bool) -> Result<u32> {
        derivation_index(&mut *self.tx, hardened).await
    }

    pub async fn max_used_derivation_index(&mut self) -> Result<Option<u32>> {
        max_used_derivation_index(&mut *self.tx).await
    }

    pub async fn p2_puzzle_hashes(&mut self) -> Result<Vec<Bytes32>> {
        p2_puzzle_hashes(&mut *self.tx).await
    }

    pub async fn synthetic_key(&mut self, p2_puzzle_hash: Bytes32) -> Result<PublicKey> {
        synthetic_key(&mut *self.tx, p2_puzzle_hash).await
    }

    pub async fn is_p2_puzzle_hash(&mut self, p2_puzzle_hash: Bytes32) -> Result<bool> {
        is_p2_puzzle_hash(&mut *self.tx, p2_puzzle_hash).await
    }
}

async fn insert_derivation(
    conn: impl SqliteExecutor<'_>,
    p2_puzzle_hash: Bytes32,
    index: u32,
    hardened: bool,
    synthetic_key: PublicKey,
) -> Result<()> {
    let p2_puzzle_hash = p2_puzzle_hash.as_ref();
    let synthetic_key = synthetic_key.to_bytes();
    let synthetic_key_ref = synthetic_key.as_ref();
    sqlx::query!(
        "
        INSERT INTO `derivations` (`p2_puzzle_hash`, `index`, `hardened`, `synthetic_key`)
        VALUES (?, ?, ?, ?)
        ",
        p2_puzzle_hash,
        index,
        hardened,
        synthetic_key_ref
    )
    .execute(conn)
    .await?;
    Ok(())
}

async fn derivation_index(conn: impl SqliteExecutor<'_>, hardened: bool) -> Result<u32> {
    Ok(sqlx::query!(
        "
        SELECT MAX(`index`) AS `max_index`
        FROM `derivations`
        WHERE `hardened` = ?
        ",
        hardened
    )
    .fetch_one(conn)
    .await?
    .max_index
    .map_or(0, |index| index + 1)
    .try_into()?)
}

async fn max_used_derivation_index(conn: impl SqliteExecutor<'_>) -> Result<Option<u32>> {
    let row = sqlx::query!(
        "
        SELECT MAX(`index`) AS `max_index`
        FROM `derivations`
        WHERE EXISTS (SELECT * FROM `coin_states` WHERE `puzzle_hash` = `p2_puzzle_hash`)
        "
    )
    .fetch_one(conn)
    .await?;
    Ok(row.max_index.map(TryInto::try_into).transpose()?)
}

async fn p2_puzzle_hashes(conn: impl SqliteExecutor<'_>) -> Result<Vec<Bytes32>> {
    let rows = sqlx::query!(
        "
        SELECT `p2_puzzle_hash`
        FROM `derivations`
        ORDER BY `index` ASC, `hardened` ASC
        "
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|row| to_bytes32(&row.p2_puzzle_hash))
        .collect::<Result<_>>()
}

async fn synthetic_key(
    conn: impl SqliteExecutor<'_>,
    p2_puzzle_hash: Bytes32,
) -> Result<PublicKey> {
    let p2_puzzle_hash = p2_puzzle_hash.as_ref();
    let row = sqlx::query!(
        "
        SELECT `synthetic_key`
        FROM `derivations`
        WHERE `p2_puzzle_hash` = ?
        ",
        p2_puzzle_hash
    )
    .fetch_one(conn)
    .await?;
    let bytes = row.synthetic_key.as_slice();
    Ok(PublicKey::from_bytes(&to_bytes(bytes)?)?)
}

async fn is_p2_puzzle_hash(conn: impl SqliteExecutor<'_>, p2_puzzle_hash: Bytes32) -> Result<bool> {
    let p2_puzzle_hash = p2_puzzle_hash.as_ref();
    Ok(sqlx::query!(
        "
        SELECT COUNT(*) AS `count` FROM `derivations` WHERE `p2_puzzle_hash` = ?
        ",
        p2_puzzle_hash
    )
    .fetch_one(conn)
    .await?
    .count
        > 0)
}