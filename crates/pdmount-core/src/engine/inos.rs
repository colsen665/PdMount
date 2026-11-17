//! Inode number allocation and reservation.

use super::*;

impl Engine {
    /// An inode number from the reserved range (hot path).
    pub(super) fn allocate_ino(&self) -> Result<Ino> {
        let mut inos = self.inos.lock();
        if inos.next > inos.reserved {
            return Err(CoreError::NeedsInos);
        }
        let ino = inos.next;
        inos.next += 1;
        Ok(ino)
    }

    /// Extends the durable reservation so the hot path rarely runs out.
    pub async fn reserve_inos(&self, at_least: u64) -> Result<()> {
        let _reserving = self.reserving.lock().await;
        let reserved = {
            let inos = self.inos.lock();
            if inos.reserved >= inos.next + at_least {
                return Ok(());
            }
            inos.next + at_least + INO_RESERVATION
        };
        self.store
            .write_durable(move |conn| meta_set(conn, "ino_reserved", &reserved.to_string()))
            .await?;
        let mut inos = self.inos.lock();
        inos.reserved = inos.reserved.max(reserved);
        Ok(())
    }

    pub(super) async fn keep_inos_reserved(&self) {
        loop {
            if let Err(error) = self.reserve_inos(INO_RESERVATION / 2).await {
                tracing::warn!(%error, "cannot reserve inode numbers");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub(crate) async fn allocate_inos(&self, count: u64) -> Result<Vec<Ino>> {
        self.reserve_inos(count).await?;
        let mut inos = self.inos.lock();
        if inos.next + count - 1 > inos.reserved {
            return Err(CoreError::NeedsInos);
        }
        let range = inos.next..inos.next + count;
        inos.next += count;
        // `apply_listing` pops from the end; hand out ascending numbers.
        Ok(range.rev().collect())
    }
}
