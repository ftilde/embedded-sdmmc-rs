//! The Volume Manager handles partitions and open files on a block device.

use byteorder::{ByteOrder, LittleEndian};
use core::convert::TryFrom;

use crate::fat::{self, BlockCache, RESERVED_ENTRIES};

use crate::filesystem::{
    Attributes, Cluster, DirEntry, Directory, File, IdGenerator, Mode, SearchId, ShortFileName,
    TimeSource, MAX_FILE_SIZE,
};
use crate::{
    debug, Block, BlockCount, BlockDevice, BlockIdx, Error, Volume, VolumeIdx, VolumeType,
    PARTITION_ID_FAT16, PARTITION_ID_FAT16_LBA, PARTITION_ID_FAT32_CHS_LBA, PARTITION_ID_FAT32_LBA,
};
use heapless::Vec;

#[derive(PartialEq, Eq)]
struct ClusterDescriptor {
    volume_idx: VolumeIdx,
    cluster: Cluster,
    search_id: SearchId,
}

impl ClusterDescriptor {
    fn new(volume_idx: VolumeIdx, cluster: Cluster, search_id: SearchId) -> Self {
        Self {
            volume_idx,
            cluster,
            search_id,
        }
    }

    fn compare_volume_and_cluster(&self, volume_idx: VolumeIdx, cluster: Cluster) -> bool {
        self.volume_idx == volume_idx && self.cluster == cluster
    }

    fn compare_id(&self, search_id: SearchId) -> bool {
        self.search_id == search_id
    }
}

/// A `VolumeManager` wraps a block device and gives access to the volumes within it.
pub struct VolumeManager<D, T, const MAX_DIRS: usize = 4, const MAX_FILES: usize = 4>
where
    D: BlockDevice,
    T: TimeSource,
    <D as BlockDevice>::Error: core::fmt::Debug,
{
    pub(crate) block_device: D,
    pub(crate) timesource: T,
    id_generator: IdGenerator,
    open_dirs: Vec<ClusterDescriptor, MAX_DIRS>,
    open_files: Vec<ClusterDescriptor, MAX_FILES>,
}

impl<D, T> VolumeManager<D, T, 4, 4>
where
    D: BlockDevice,
    T: TimeSource,
    <D as BlockDevice>::Error: core::fmt::Debug,
{
    /// Create a new Volume Manager using a generic `BlockDevice`. From this
    /// object we can open volumes (partitions) and with those we can open
    /// files.
    ///
    /// This creates a `VolumeManager` with default values
    /// MAX_DIRS = 4, MAX_FILES = 4. Call `VolumeManager::new_with_limits(block_device, timesource)`
    /// if you need different limits.
    pub fn new(block_device: D, timesource: T) -> VolumeManager<D, T, 4, 4> {
        Self::new_with_limits(block_device, timesource)
    }
}

impl<D, T, const MAX_DIRS: usize, const MAX_FILES: usize> VolumeManager<D, T, MAX_DIRS, MAX_FILES>
where
    D: BlockDevice,
    T: TimeSource,
    <D as BlockDevice>::Error: core::fmt::Debug,
{
    /// Create a new Volume Manager using a generic `BlockDevice`. From this
    /// object we can open volumes (partitions) and with those we can open
    /// files.
    pub fn new_with_limits(
        block_device: D,
        timesource: T,
    ) -> VolumeManager<D, T, MAX_DIRS, MAX_FILES> {
        debug!("Creating new embedded-sdmmc::VolumeManager");
        VolumeManager {
            block_device,
            timesource,
            id_generator: IdGenerator::new(),
            open_dirs: Vec::new(),
            open_files: Vec::new(),
        }
    }

    /// Temporarily get access to the underlying block device.
    pub fn device(&mut self) -> &mut D {
        &mut self.block_device
    }

    /// Get a volume (or partition) based on entries in the Master Boot
    /// Record. We do not support GUID Partition Table disks. Nor do we
    /// support any concept of drive letters - that is for a higher layer to
    /// handle.
    pub fn get_volume(&mut self, volume_idx: VolumeIdx) -> Result<Volume, Error<D::Error>> {
        const PARTITION1_START: usize = 446;
        const PARTITION2_START: usize = PARTITION1_START + PARTITION_INFO_LENGTH;
        const PARTITION3_START: usize = PARTITION2_START + PARTITION_INFO_LENGTH;
        const PARTITION4_START: usize = PARTITION3_START + PARTITION_INFO_LENGTH;
        const FOOTER_START: usize = 510;
        const FOOTER_VALUE: u16 = 0xAA55;
        const PARTITION_INFO_LENGTH: usize = 16;
        const PARTITION_INFO_STATUS_INDEX: usize = 0;
        const PARTITION_INFO_TYPE_INDEX: usize = 4;
        const PARTITION_INFO_LBA_START_INDEX: usize = 8;
        const PARTITION_INFO_NUM_BLOCKS_INDEX: usize = 12;

        let (part_type, lba_start, num_blocks) = {
            let mut blocks = [Block::new()];
            self.block_device
                .read(&mut blocks, BlockIdx(0), "read_mbr")
                .map_err(Error::DeviceError)?;
            let block = &blocks[0];
            // We only support Master Boot Record (MBR) partitioned cards, not
            // GUID Partition Table (GPT)
            if LittleEndian::read_u16(&block[FOOTER_START..FOOTER_START + 2]) != FOOTER_VALUE {
                return Err(Error::FormatError("Invalid MBR signature"));
            }
            let partition = match volume_idx {
                VolumeIdx(0) => {
                    &block[PARTITION1_START..(PARTITION1_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(1) => {
                    &block[PARTITION2_START..(PARTITION2_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(2) => {
                    &block[PARTITION3_START..(PARTITION3_START + PARTITION_INFO_LENGTH)]
                }
                VolumeIdx(3) => {
                    &block[PARTITION4_START..(PARTITION4_START + PARTITION_INFO_LENGTH)]
                }
                _ => {
                    return Err(Error::NoSuchVolume);
                }
            };
            // Only 0x80 and 0x00 are valid (bootable, and non-bootable)
            if (partition[PARTITION_INFO_STATUS_INDEX] & 0x7F) != 0x00 {
                return Err(Error::FormatError("Invalid partition status"));
            }
            let lba_start = LittleEndian::read_u32(
                &partition[PARTITION_INFO_LBA_START_INDEX..(PARTITION_INFO_LBA_START_INDEX + 4)],
            );
            let num_blocks = LittleEndian::read_u32(
                &partition[PARTITION_INFO_NUM_BLOCKS_INDEX..(PARTITION_INFO_NUM_BLOCKS_INDEX + 4)],
            );
            (
                partition[PARTITION_INFO_TYPE_INDEX],
                BlockIdx(lba_start),
                BlockCount(num_blocks),
            )
        };
        match part_type {
            PARTITION_ID_FAT32_CHS_LBA
            | PARTITION_ID_FAT32_LBA
            | PARTITION_ID_FAT16_LBA
            | PARTITION_ID_FAT16 => {
                let volume = fat::parse_volume(self, lba_start, num_blocks)?;
                Ok(Volume {
                    idx: volume_idx,
                    volume_type: volume,
                })
            }
            _ => Err(Error::FormatError("Partition type not supported")),
        }
    }

    /// Open the volume's root directory.
    ///
    /// You can then read the directory entries with `iterate_dir` and `open_file_in_dir`.
    ///
    /// TODO: Work out how to prevent damage occuring to the file system while
    /// this directory handle is open. In particular, stop this directory
    /// being unlinked.
    pub fn open_root_dir(&mut self, volume: &Volume) -> Result<Directory, Error<D::Error>> {
        if self.open_dirs.is_full() {
            return Err(Error::TooManyOpenDirs);
        }

        // Find a free directory entry, and check the root dir isn't open. As
        // we already know the root dir's magic cluster number, we can do both
        // checks in one loop.
        if cluster_already_open(&self.open_dirs, volume.idx, Cluster::ROOT_DIR) {
            return Err(Error::DirAlreadyOpen);
        }

        let search_id = self.id_generator.get();
        // Remember this open directory
        self.open_dirs
            .push(ClusterDescriptor::new(
                volume.idx,
                Cluster::ROOT_DIR,
                search_id,
            ))
            .map_err(|_| Error::TooManyOpenDirs)?;

        Ok(Directory {
            cluster: Cluster::ROOT_DIR,
            search_id,
        })
    }

    /// Open a directory.
    ///
    /// You can then read the directory entries with `iterate_dir` and `open_file_in_dir`.
    ///
    /// TODO: Work out how to prevent damage occuring to the file system while
    /// this directory handle is open. In particular, stop this directory
    /// being unlinked.
    pub fn open_dir(
        &mut self,
        volume: &Volume,
        parent_dir: &Directory,
        name: &str,
    ) -> Result<Directory, Error<D::Error>> {
        if self.open_dirs.is_full() {
            return Err(Error::TooManyOpenDirs);
        }

        // Open the directory
        let dir_entry = match &volume.volume_type {
            VolumeType::Fat(fat) => fat.find_directory_entry(self, parent_dir, name)?,
        };

        if !dir_entry.attributes.is_directory() {
            return Err(Error::OpenedDirAsFile);
        }

        // Check it's not already open
        if cluster_already_open(&self.open_dirs, volume.idx, dir_entry.cluster) {
            return Err(Error::DirAlreadyOpen);
        }

        // Remember this open directory.
        let search_id = self.id_generator.get();
        self.open_dirs
            .push(ClusterDescriptor::new(
                volume.idx,
                dir_entry.cluster,
                search_id,
            ))
            .map_err(|_| Error::TooManyOpenDirs)?;

        Ok(Directory {
            cluster: dir_entry.cluster,
            search_id,
        })
    }

    /// Close a directory. You cannot perform operations on an open directory
    /// and so must close it if you want to do something with it.
    pub fn close_dir(&mut self, volume: &Volume, dir: Directory) {
        // Unwrap, because we should never be in a situation where we're attempting to close a dir
        // with an ID which doesn't exist in our open dirs list.
        let idx_to_close = cluster_position_by_id(&self.open_dirs, dir.search_id).unwrap();
        self.open_dirs.remove(idx_to_close);
    }

    /// Look in a directory for a named file.
    pub fn find_directory_entry(
        &mut self,
        volume: &Volume,
        dir: &Directory,
        name: &str,
    ) -> Result<DirEntry, Error<D::Error>> {
        match &volume.volume_type {
            VolumeType::Fat(fat) => fat.find_directory_entry(self, dir, name),
        }
    }

    /// Call a callback function for each directory entry in a directory.
    pub fn iterate_dir<F>(
        &mut self,
        volume: &Volume,
        dir: &Directory,
        func: F,
    ) -> Result<(), Error<D::Error>>
    where
        F: FnMut(&DirEntry),
    {
        match &volume.volume_type {
            VolumeType::Fat(fat) => fat.iterate_dir(self, dir, func),
        }
    }

    /// Open a file from DirEntry. This is obtained by calling iterate_dir. A file can only be opened once.
    pub fn open_dir_entry(
        &mut self,
        volume: &mut Volume,
        dir_entry: DirEntry,
        mode: Mode,
    ) -> Result<File, Error<D::Error>> {
        if self.open_files.is_full() {
            return Err(Error::TooManyOpenFiles);
        }

        // Check it's not already open
        if cluster_already_open(&self.open_dirs, volume.idx, dir_entry.cluster) {
            return Err(Error::DirAlreadyOpen);
        }

        if dir_entry.attributes.is_directory() {
            return Err(Error::OpenedDirAsFile);
        }
        if dir_entry.attributes.is_read_only() && mode != Mode::ReadOnly {
            return Err(Error::ReadOnly);
        }

        let mode = solve_mode_variant(mode, true);
        let search_id = self.id_generator.get();

        let file = match mode {
            Mode::ReadOnly => File {
                starting_cluster: dir_entry.cluster,
                current_cluster: (0, dir_entry.cluster),
                current_offset: 0,
                length: dir_entry.size,
                mode,
                entry: dir_entry,
                search_id,
            },
            Mode::ReadWriteAppend => {
                let mut file = File {
                    starting_cluster: dir_entry.cluster,
                    current_cluster: (0, dir_entry.cluster),
                    current_offset: 0,
                    length: dir_entry.size,
                    mode,
                    entry: dir_entry,
                    search_id,
                };
                // seek_from_end with 0 can't fail
                file.seek_from_end(0).ok();
                file
            }
            Mode::ReadWriteTruncate => {
                let mut file = File {
                    starting_cluster: dir_entry.cluster,
                    current_cluster: (0, dir_entry.cluster),
                    current_offset: 0,
                    length: dir_entry.size,
                    mode,
                    entry: dir_entry,
                    search_id,
                };
                match &mut volume.volume_type {
                    VolumeType::Fat(fat) => {
                        fat.truncate_cluster_chain(self, file.starting_cluster)?
                    }
                };
                file.update_length(0);
                // TODO update entry Timestamps
                match &volume.volume_type {
                    VolumeType::Fat(fat) => {
                        let fat_type = fat.get_fat_type();
                        self.write_entry_to_disk(fat_type, &file.entry)?;
                    }
                };

                file
            }
            _ => return Err(Error::Unsupported),
        };

        // Remember this open file
        self.open_files
            .push(ClusterDescriptor::new(
                volume.idx,
                file.starting_cluster,
                search_id,
            ))
            .map_err(|_| Error::TooManyOpenDirs)?;

        Ok(file)
    }

    /// Open a file with the given full path. A file can only be opened once.
    pub fn open_file_in_dir(
        &mut self,
        volume: &mut Volume,
        dir: &Directory,
        name: &str,
        mode: Mode,
    ) -> Result<File, Error<D::Error>> {
        let dir_entry = match &volume.volume_type {
            VolumeType::Fat(fat) => fat.find_directory_entry(self, dir, name),
        };

        if self.open_files.is_full() {
            return Err(Error::TooManyOpenFiles);
        }

        let dir_entry = match dir_entry {
            Ok(entry) => Some(entry),
            Err(_)
                if (mode == Mode::ReadWriteCreate)
                    | (mode == Mode::ReadWriteCreateOrTruncate)
                    | (mode == Mode::ReadWriteCreateOrAppend) =>
            {
                None
            }
            _ => return Err(Error::FileNotFound),
        };

        if let Some(d) = &dir_entry {
            if cluster_already_open(&self.open_files, volume.idx, d.cluster) {
                return Err(Error::FileAlreadyOpen);
            }
        }

        let mode = solve_mode_variant(mode, dir_entry.is_some());

        match mode {
            Mode::ReadWriteCreate => {
                if dir_entry.is_some() {
                    return Err(Error::FileAlreadyExists);
                }
                let file_name =
                    ShortFileName::create_from_str(name).map_err(Error::FilenameError)?;
                let att = Attributes::create_from_fat(0);
                let entry = match &mut volume.volume_type {
                    VolumeType::Fat(fat) => {
                        fat.write_new_directory_entry(self, dir, file_name, att)?
                    }
                };

                let search_id = self.id_generator.get();

                let file = File {
                    starting_cluster: entry.cluster,
                    current_cluster: (0, entry.cluster),
                    current_offset: 0,
                    length: entry.size,
                    mode,
                    entry,
                    search_id,
                };

                // Remember this open file
                self.open_files
                    .push(ClusterDescriptor::new(
                        volume.idx,
                        file.starting_cluster,
                        search_id,
                    ))
                    .map_err(|_| Error::TooManyOpenFiles)?;

                Ok(file)
            }
            _ => {
                // Safe to unwrap, since we actually have an entry if we got here
                let dir_entry = dir_entry.unwrap();
                // FIXME: if 2 files are in the same cluster this will cause an error when opening
                // a file for a first time in a different than `ReadWriteCreate` mode.
                self.open_dir_entry(volume, dir_entry, mode)
            }
        }
    }

    /// Delete a closed file with the given full path, if exists.
    pub fn delete_file_in_dir(
        &mut self,
        volume: &Volume,
        dir: &Directory,
        name: &str,
    ) -> Result<(), Error<D::Error>> {
        debug!(
            "delete_file(volume={:?}, dir={:?}, filename={:?}",
            volume, dir, name
        );
        let dir_entry = match &volume.volume_type {
            VolumeType::Fat(fat) => fat.find_directory_entry(self, dir, name),
        }?;

        if dir_entry.attributes.is_directory() {
            return Err(Error::DeleteDirAsFile);
        }

        if cluster_already_open(&self.open_files, volume.idx, dir_entry.cluster) {
            return Err(Error::FileIsOpen);
        }

        match &volume.volume_type {
            VolumeType::Fat(fat) => fat.delete_directory_entry(self, dir, name)?,
        }

        // Unwrap, because we should never be in a situation where we're attempting to close a file
        // which doesn't exist in our open files list.
        let idx_to_remove = self
            .open_files
            .iter()
            .position(|d| d.compare_volume_and_cluster(volume.idx, dir_entry.cluster))
            .unwrap();
        self.open_files.remove(idx_to_remove);

        Ok(())
    }

    /// Read from an open file.
    pub fn read(
        &mut self,
        volume: &Volume,
        file: &mut File,
        buffer: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        // Calculate which file block the current offset lies within
        // While there is more to read, read the block and copy in to the buffer.
        // If we need to find the next cluster, walk the FAT.
        let mut space = buffer.len();
        let mut read = 0;
        while space > 0 && !file.eof() {
            let (block_idx, block_offset, block_avail) =
                self.find_data_on_disk(volume, &mut file.current_cluster, file.current_offset)?;
            let mut blocks = [Block::new()];
            self.block_device
                .read(&mut blocks, block_idx, "read")
                .map_err(Error::DeviceError)?;
            let block = &blocks[0];
            let to_copy = block_avail.min(space).min(file.left() as usize);
            assert!(to_copy != 0);
            buffer[read..read + to_copy]
                .copy_from_slice(&block[block_offset..block_offset + to_copy]);
            read += to_copy;
            space -= to_copy;
            file.seek_from_current(to_copy as i32).unwrap();
        }
        Ok(read)
    }

    /// Write to a open file.
    pub fn write(
        &mut self,
        volume: &mut Volume,
        file: &mut File,
        buffer: &[u8],
    ) -> Result<usize, Error<D::Error>> {
        #[cfg(feature = "defmt-log")]
        debug!(
            "write(volume={:?}, file={:?}, buffer={:x}",
            volume, file, buffer
        );

        #[cfg(feature = "log")]
        debug!(
            "write(volume={:?}, file={:?}, buffer={:x?}",
            volume, file, buffer
        );

        if file.mode == Mode::ReadOnly {
            return Err(Error::ReadOnly);
        }
        if file.starting_cluster.0 < RESERVED_ENTRIES {
            // file doesn't have a valid allocated cluster (possible zero-length file), allocate one
            file.starting_cluster = match &mut volume.volume_type {
                VolumeType::Fat(fat) => fat.alloc_cluster(self, None, false)?,
            };

            // Update the cluster descriptor in our open files list
            let cluster_to_update = self
                .open_files
                .iter_mut()
                .find(|d| d.compare_id(file.search_id));

            if let Some(c) = cluster_to_update {
                c.cluster = file.starting_cluster;
            }

            file.entry.cluster = file.starting_cluster;
            debug!("Alloc first cluster {:?}", file.starting_cluster);
        }
        if (file.current_cluster.1).0 < file.starting_cluster.0 {
            debug!("Rewinding to start");
            file.current_cluster = (0, file.starting_cluster);
        }
        let bytes_until_max = usize::try_from(MAX_FILE_SIZE - file.current_offset)
            .map_err(|_| Error::ConversionError)?;
        let bytes_to_write = core::cmp::min(buffer.len(), bytes_until_max);
        let mut written = 0;

        while written < bytes_to_write {
            let mut current_cluster = file.current_cluster;
            debug!(
                "Have written bytes {}/{}, finding cluster {:?}",
                written, bytes_to_write, current_cluster
            );
            let (block_idx, block_offset, block_avail) =
                match self.find_data_on_disk(volume, &mut current_cluster, file.current_offset) {
                    Ok(vars) => {
                        debug!(
                            "Found block_idx={:?}, block_offset={:?}, block_avail={}",
                            vars.0, vars.1, vars.2
                        );
                        vars
                    }
                    Err(Error::EndOfFile) => {
                        debug!("Extending file");
                        match &mut volume.volume_type {
                            VolumeType::Fat(ref mut fat) => {
                                if fat
                                    .alloc_cluster(self, Some(current_cluster.1), false)
                                    .is_err()
                                {
                                    return Ok(written);
                                }
                                debug!("Allocated new FAT cluster, finding offsets...");
                                let new_offset = self
                                    .find_data_on_disk(
                                        volume,
                                        &mut current_cluster,
                                        file.current_offset,
                                    )
                                    .map_err(|_| Error::AllocationError)?;
                                debug!("New offset {:?}", new_offset);
                                new_offset
                            }
                        }
                    }
                    Err(e) => return Err(e),
                };
            let mut blocks = [Block::new()];
            let to_copy = core::cmp::min(block_avail, bytes_to_write - written);
            if block_offset != 0 {
                debug!("Partial block write");
                self.block_device
                    .read(&mut blocks, block_idx, "read")
                    .map_err(Error::DeviceError)?;
            }
            let block = &mut blocks[0];
            block[block_offset..block_offset + to_copy]
                .copy_from_slice(&buffer[written..written + to_copy]);
            debug!("Writing block {:?}", block_idx);
            self.block_device
                .write(&blocks, block_idx)
                .map_err(Error::DeviceError)?;
            written += to_copy;
            file.current_cluster = current_cluster;
            let to_copy = i32::try_from(to_copy).map_err(|_| Error::ConversionError)?;
            // TODO: Should we do this once when the whole file is written?
            file.update_length(file.length + (to_copy as u32));
            file.seek_from_current(to_copy).unwrap();
            file.entry.attributes.set_archive(true);
            file.entry.mtime = self.timesource.get_timestamp();
            debug!("Updating FAT info sector");
            match &mut volume.volume_type {
                VolumeType::Fat(fat) => {
                    fat.update_info_sector(self)?;
                    debug!("Updating dir entry");
                    self.write_entry_to_disk(fat.get_fat_type(), &file.entry)?;
                }
            }
        }
        Ok(written)
    }

    /// Close a file with the given full path.
    pub fn close_file(&mut self, volume: &Volume, file: File) -> Result<(), Error<D::Error>> {
        // Unwrap, because we should never be in a situation where we're attempting to close a file
        // with an ID which doesn't exist in our open files list.
        let idx_to_close = cluster_position_by_id(&self.open_files, file.search_id).unwrap();
        self.open_files.remove(idx_to_close);

        Ok(())
    }

    /// Check if any files or folders are open.
    pub fn has_open_handles(&self) -> bool {
        !(self.open_dirs.is_empty() || self.open_files.is_empty())
    }

    /// Consume self and return BlockDevice and TimeSource
    pub fn free(self) -> (D, T) {
        (self.block_device, self.timesource)
    }

    /// This function turns `desired_offset` into an appropriate block to be
    /// read. It either calculates this based on the start of the file, or
    /// from the last cluster we read - whichever is better.
    fn find_data_on_disk(
        &mut self,
        volume: &Volume,
        start: &mut (u32, Cluster),
        desired_offset: u32,
    ) -> Result<(BlockIdx, usize, usize), Error<D::Error>> {
        let bytes_per_cluster = match &volume.volume_type {
            VolumeType::Fat(fat) => fat.bytes_per_cluster(),
        };
        // How many clusters forward do we need to go?
        let offset_from_cluster = desired_offset - start.0;
        let num_clusters = offset_from_cluster / bytes_per_cluster;
        let mut block_cache = BlockCache::empty();
        for _ in 0..num_clusters {
            start.1 = match &volume.volume_type {
                VolumeType::Fat(fat) => fat.next_cluster(self, start.1, &mut block_cache)?,
            };
            start.0 += bytes_per_cluster;
        }
        // How many blocks in are we?
        let offset_from_cluster = desired_offset - start.0;
        assert!(offset_from_cluster < bytes_per_cluster);
        let num_blocks = BlockCount(offset_from_cluster / Block::LEN_U32);
        let block_idx = match &volume.volume_type {
            VolumeType::Fat(fat) => fat.cluster_to_block(start.1),
        } + num_blocks;
        let block_offset = (desired_offset % Block::LEN_U32) as usize;
        let available = Block::LEN - block_offset;
        Ok((block_idx, block_offset, available))
    }

    /// Writes a Directory Entry to the disk
    fn write_entry_to_disk(
        &mut self,
        fat_type: fat::FatType,
        entry: &DirEntry,
    ) -> Result<(), Error<D::Error>> {
        let mut blocks = [Block::new()];
        self.block_device
            .read(&mut blocks, entry.entry_block, "read")
            .map_err(Error::DeviceError)?;
        let block = &mut blocks[0];

        let start = usize::try_from(entry.entry_offset).map_err(|_| Error::ConversionError)?;
        block[start..start + 32].copy_from_slice(&entry.serialize(fat_type)[..]);

        self.block_device
            .write(&blocks, entry.entry_block)
            .map_err(Error::DeviceError)?;
        Ok(())
    }
}

fn cluster_position_by_id(
    cluster_list: &[ClusterDescriptor],
    id_to_find: SearchId,
) -> Option<usize> {
    cluster_list.iter().position(|f| f.compare_id(id_to_find))
}

fn cluster_already_open(
    cluster_list: &[ClusterDescriptor],
    volume_idx: VolumeIdx,
    cluster: Cluster,
) -> bool {
    cluster_list
        .iter()
        .any(|d| d.compare_volume_and_cluster(volume_idx, cluster))
}

/// Transform mode variants (ReadWriteCreate_Or_Append) to simple modes ReadWriteAppend or
/// ReadWriteCreate
fn solve_mode_variant(mode: Mode, dir_entry_is_some: bool) -> Mode {
    let mut mode = mode;
    if mode == Mode::ReadWriteCreateOrAppend {
        if dir_entry_is_some {
            mode = Mode::ReadWriteAppend;
        } else {
            mode = Mode::ReadWriteCreate;
        }
    } else if mode == Mode::ReadWriteCreateOrTruncate {
        if dir_entry_is_some {
            mode = Mode::ReadWriteTruncate;
        } else {
            mode = Mode::ReadWriteCreate;
        }
    }
    mode
}
