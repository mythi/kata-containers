// Copyright (c) 2019 Ant Financial
// Copyright (c) 2023 Alibaba Cloud
//
// SPDX-License-Identifier: Apache-2.0
//

use crate::linux_abi::pcipath_from_dev_tree_path;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
#[cfg(target_arch = "s390x")]
use kata_types::device::DRIVER_BLK_CCW_TYPE;
use kata_types::device::{
    DRIVER_BLK_MMIO_TYPE, DRIVER_BLK_PCI_TYPE, DRIVER_NVDIMM_TYPE, DRIVER_SCSI_TYPE,
};
use kata_types::mount::{StorageDevice, KATA_BLOCK_VOLUME_CREATE_FS};
use nix::sys::stat::{major, minor};
use protocols::agent::Storage;
use tracing::instrument;

#[cfg(target_arch = "s390x")]
use crate::ccw;
#[cfg(target_arch = "s390x")]
use crate::device::block_device_handler::get_virtio_blk_ccw_device_name;
use crate::device::block_device_handler::{
    get_virtio_blk_mmio_device_name, get_virtio_blk_pci_device_name,
};
use crate::device::nvdimm_device_handler::wait_for_pmem_device;
use crate::device::scsi_device_handler::get_scsi_device_name;
use crate::sandbox::Sandbox;
use crate::storage::{
    common_storage_handler, new_device, set_ownership, StorageContext, StorageHandler,
};
use slog::Logger;
#[cfg(target_arch = "s390x")]
use std::str::FromStr;
use tokio::sync::Mutex;

const EPHEMERAL_ENCRYPTION_DRIVER_OPTION: &str = "encryption_key=ephemeral";
const MKFS_EXT4: &str = "mkfs.ext4";
const BLOCK_EMPTYDIR_EXT4_MKFS_OPTS: [&str; 8] =
    ["-O", "^has_journal", "-m", "0", "-i", "163840", "-I", "128"];

#[derive(Debug, Eq, PartialEq)]
struct BlockStorageDriverOptions {
    has_ephemeral_encryption: bool,
    should_create_filesystem: bool,
}

fn get_device_number(dev_path: &str, metadata: Option<&fs::Metadata>) -> Result<String> {
    let dev_id = match metadata {
        Some(m) => m.rdev(),
        None => {
            let m =
                fs::metadata(dev_path).context(format!("get metadata on file {:?}", dev_path))?;
            m.rdev()
        }
    };
    Ok(format!("{}:{}", major(dev_id), minor(dev_id)))
}

pub(crate) async fn get_block_device_number_for_storage(
    storage: &mut Storage,
    sandbox: &Arc<Mutex<Sandbox>>,
) -> Result<String> {
    match storage.driver.as_str() {
        DRIVER_BLK_MMIO_TYPE => {
            if !Path::new(&storage.source).exists() {
                get_virtio_blk_mmio_device_name(sandbox, &storage.source)
                    .await
                    .context("failed to get mmio device name")?;
            }
            get_device_number(&storage.source, None)
        }
        DRIVER_BLK_PCI_TYPE => {
            if storage.source.starts_with("/dev") {
                let metadata = fs::metadata(&storage.source)
                    .context(format!("get metadata on file {:?}", &storage.source))?;
                let mode = metadata.permissions().mode();
                if mode & libc::S_IFBLK == 0 {
                    return Err(anyhow!("Invalid device {}", &storage.source));
                }
                get_device_number(&storage.source, Some(&metadata))
            } else {
                let (root_complex, pcipath) = pcipath_from_dev_tree_path(&storage.source)?;
                let dev_path = get_virtio_blk_pci_device_name(sandbox, root_complex, &pcipath)
                    .await
                    .context("failed to get pci device name")?;
                storage.source = dev_path;
                get_device_number(&storage.source, None)
            }
        }
        DRIVER_SCSI_TYPE => {
            let dev_path = get_scsi_device_name(sandbox, &storage.source).await?;
            storage.source = dev_path.clone();
            get_device_number(&dev_path, None)
        }
        #[cfg(target_arch = "s390x")]
        DRIVER_BLK_CCW_TYPE => {
            let ccw_device = ccw::Device::from_str(&storage.source)?;
            let dev_path = get_virtio_blk_ccw_device_name(sandbox, &ccw_device).await?;
            storage.source = dev_path;
            get_device_number(&storage.source, None)
        }
        _ => Err(anyhow!(
            "unsupported trusted storage block driver {}",
            storage.driver
        )),
    }
}

pub(crate) fn is_ephemeral_encrypted_block_storage(storage: &Storage) -> bool {
    storage
        .driver_options
        .iter()
        .any(|opt| opt == EPHEMERAL_ENCRYPTION_DRIVER_OPTION)
}

async fn handle_block_storage(
    logger: &Logger,
    storage: &Storage,
    dev_num: &str,
) -> Result<Arc<dyn StorageDevice>> {
    let options = block_storage_driver_options(storage)?;

    if options.has_ephemeral_encryption {
        let mkfs_opts = BLOCK_EMPTYDIR_EXT4_MKFS_OPTS.join(" ");
        crate::rpc::cdh_secure_mount(
            "block-device",
            dev_num,
            "luks2",
            &storage.mount_point,
            &mkfs_opts,
        )
        .await?;
        set_ownership(logger, storage)?;
        new_device(storage.mount_point.clone())
    } else {
        if options.should_create_filesystem {
            ensure_block_filesystem(logger, storage).await?;
        }
        let path = common_storage_handler(logger, storage)?;
        new_device(path)
    }
}

fn block_storage_driver_options(storage: &Storage) -> Result<BlockStorageDriverOptions> {
    let has_ephemeral_encryption = storage
        .driver_options
        .iter()
        .any(|opt| opt == EPHEMERAL_ENCRYPTION_DRIVER_OPTION);
    let should_create_filesystem = should_create_block_filesystem(storage);

    if has_ephemeral_encryption && !should_create_filesystem {
        return Err(anyhow!(
            "{} requires {} for block storage",
            EPHEMERAL_ENCRYPTION_DRIVER_OPTION,
            KATA_BLOCK_VOLUME_CREATE_FS
        ));
    }

    Ok(BlockStorageDriverOptions {
        has_ephemeral_encryption,
        should_create_filesystem,
    })
}

fn should_create_block_filesystem(storage: &Storage) -> bool {
    storage
        .driver_options
        .iter()
        .any(|opt| opt == KATA_BLOCK_VOLUME_CREATE_FS)
}

async fn ensure_block_filesystem(logger: &Logger, storage: &Storage) -> Result<()> {
    match storage.fstype.as_str() {
        "ext4" => ensure_ext4_filesystem(logger, &storage.source).await,
        _ => Err(anyhow!(
            "creating filesystem {} for block storage is unsupported",
            storage.fstype
        )),
    }
}

async fn ensure_ext4_filesystem(logger: &Logger, source: &str) -> Result<()> {
    // This option is emitted for block emptyDir volumes, whose backing device
    // is ephemeral and freshly allocated for the pod.
    info!(logger, "creating ext4 filesystem"; "source" => source);
    let output = {
        // Keep the agent SIGCHLD handler from reaping this child before
        // tokio::process observes it.
        let _locker = rustjail::container::WAIT_PID_LOCKER.lock().await;
        // BLOCK_EMPTYDIR_EXT4_MKFS_OPTS mirrors CDH's EXT4_INTEGRITY_MKFS_OPTS
        // from confidential-data-hub/hub/src/storage/volume_type/blockdevice/mod.rs.
        // CDH's FsFormatter adds "-F" and its mapped device path separately in
        // confidential-data-hub/hub/src/storage/drivers/filesystem.rs; here the
        // agent invokes mkfs.ext4 directly, so add "-F" and source below.
        tokio::process::Command::new(MKFS_EXT4)
            .arg("-F")
            .args(BLOCK_EMPTYDIR_EXT4_MKFS_OPTS)
            .arg(source)
            .output()
            .await
            .with_context(|| format!("run {MKFS_EXT4} for {source}"))?
    };

    if output.status.success() {
        return Ok(());
    }

    Err(anyhow!(
        "{} failed for {}: status={}, stdout={}, stderr={}",
        MKFS_EXT4,
        source,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage_with_driver_options(options: &[&str]) -> Storage {
        Storage {
            driver_options: options.iter().map(|opt| opt.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn block_storage_options_allow_normal_existing_storage() {
        let storage = storage_with_driver_options(&[]);

        let options = block_storage_driver_options(&storage).unwrap();

        assert_eq!(
            options,
            BlockStorageDriverOptions {
                has_ephemeral_encryption: false,
                should_create_filesystem: false,
            }
        );
    }

    #[test]
    fn block_storage_options_allow_plain_fresh_storage() {
        let storage = storage_with_driver_options(&[KATA_BLOCK_VOLUME_CREATE_FS]);

        let options = block_storage_driver_options(&storage).unwrap();

        assert_eq!(
            options,
            BlockStorageDriverOptions {
                has_ephemeral_encryption: false,
                should_create_filesystem: true,
            }
        );
    }

    #[test]
    fn block_storage_options_allow_encrypted_fresh_storage() {
        let storage = storage_with_driver_options(&[
            EPHEMERAL_ENCRYPTION_DRIVER_OPTION,
            KATA_BLOCK_VOLUME_CREATE_FS,
        ]);

        let options = block_storage_driver_options(&storage).unwrap();

        assert_eq!(
            options,
            BlockStorageDriverOptions {
                has_ephemeral_encryption: true,
                should_create_filesystem: true,
            }
        );
    }

    #[test]
    fn block_storage_options_reject_encryption_without_filesystem_creation() {
        let storage = storage_with_driver_options(&[EPHEMERAL_ENCRYPTION_DRIVER_OPTION]);

        let err = block_storage_driver_options(&storage).unwrap_err();

        assert!(err.to_string().contains(KATA_BLOCK_VOLUME_CREATE_FS));
    }
}

#[derive(Debug)]
pub struct VirtioBlkMmioHandler {}

#[async_trait::async_trait]
impl StorageHandler for VirtioBlkMmioHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_BLK_MMIO_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        mut storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let dev_num = get_block_device_number_for_storage(&mut storage, ctx.sandbox).await?;
        handle_block_storage(ctx.logger, &storage, &dev_num).await
    }
}

#[derive(Debug)]
pub struct VirtioBlkPciHandler {}

#[async_trait::async_trait]
impl StorageHandler for VirtioBlkPciHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_BLK_PCI_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        mut storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let dev_num = get_block_device_number_for_storage(&mut storage, ctx.sandbox).await?;
        handle_block_storage(ctx.logger, &storage, &dev_num).await
    }
}

#[cfg(target_arch = "s390x")]
#[derive(Debug)]
pub struct VirtioBlkCcwHandler {}

#[cfg(target_arch = "s390x")]
#[async_trait::async_trait]
impl StorageHandler for VirtioBlkCcwHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_BLK_CCW_TYPE]
    }

    #[cfg(target_arch = "s390x")]
    #[instrument]
    async fn create_device(
        &self,
        mut storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let dev_num = get_block_device_number_for_storage(&mut storage, ctx.sandbox).await?;
        handle_block_storage(ctx.logger, &storage, &dev_num).await
    }

    #[cfg(not(target_arch = "s390x"))]
    #[instrument]
    async fn create_device(
        &self,
        _storage: Storage,
        _ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        Err(anyhow!("CCW is only supported on s390x"))
    }
}

#[derive(Debug)]
pub struct ScsiHandler {}

#[async_trait::async_trait]
impl StorageHandler for ScsiHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_SCSI_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        mut storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        let dev_num = get_block_device_number_for_storage(&mut storage, ctx.sandbox).await?;
        handle_block_storage(ctx.logger, &storage, &dev_num).await
    }
}

#[derive(Debug)]
pub struct PmemHandler {}

#[async_trait::async_trait]
impl StorageHandler for PmemHandler {
    #[instrument]
    fn driver_types(&self) -> &[&str] {
        &[DRIVER_NVDIMM_TYPE]
    }

    #[instrument]
    async fn create_device(
        &self,
        storage: Storage,
        ctx: &mut StorageContext,
    ) -> Result<Arc<dyn StorageDevice>> {
        // Retrieve the device for pmem storage
        wait_for_pmem_device(ctx.sandbox, &storage.source).await?;

        let path = common_storage_handler(ctx.logger, &storage)?;
        new_device(path)
    }
}
