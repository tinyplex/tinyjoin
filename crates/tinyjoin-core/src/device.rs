#[cfg(test)]
use crate::{EngineError, MAX_PAGE_COUNT};
use crate::{PAGE_SIZE, PageId, Result};

/// Synchronous durable storage used by the copy-on-write pager.
///
/// Implementations must expose a dense sequence of complete 4 KiB pages. A
/// write may replace an existing page or append exactly `page_count()`; gaps
/// are rejected. Reads observe successful writes immediately. `flush()` is a
/// device-wide durability barrier for every successful preceding write.
///
/// Callers only overwrite pages which are unreachable from the active
/// superblock. A failed write may have installed all, part, or none of the
/// source bytes in its target page and must report that uncertainty, but it
/// must not modify any other page. A failed flush may have made all, some, or
/// none of the preceding writes durable; the pager's publication state machine
/// determines whether reopening is required.
pub trait PageDevice {
    fn page_count(&self) -> PageId;
    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()>;
    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()>;

    /// Writes `source`, a whole number of pages, to consecutive pages from `first`, as that many
    /// [`Self::write_page`] calls in order would, and may make them as one call to storage. A
    /// failure may have written any of the pages, but no other.
    fn write_pages(&mut self, first: PageId, source: &[u8]) -> Result<()> {
        for (offset, page) in source.chunks(PAGE_SIZE).enumerate() {
            self.write_page(first + offset as PageId, page)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()>;
}

#[derive(Clone, Debug)]
#[cfg(test)]
pub(crate) struct MemoryPageDevice {
    pages: Vec<[u8; PAGE_SIZE]>,
    flush_count: u64,
    /// The page each write went to, in order.
    writes: Vec<PageId>,
    /// The first page and page count of each call that wrote pages, in order.
    calls: Vec<(PageId, usize)>,
}

#[cfg(test)]
impl MemoryPageDevice {
    pub(crate) fn new(page_count: PageId) -> Result<Self> {
        if page_count > MAX_PAGE_COUNT {
            return Err(device_error(format!(
                "Memory page count {page_count} exceeds the {MAX_PAGE_COUNT}-page limit"
            )));
        }
        Ok(Self {
            pages: vec![[0; PAGE_SIZE]; page_count as usize],
            flush_count: 0,
            writes: Vec::new(),
            calls: Vec::new(),
        })
    }

    pub(crate) fn page(&self, id: PageId) -> Result<&[u8; PAGE_SIZE]> {
        let page_count = self.page_count();
        if id >= page_count {
            return Err(out_of_range(id, page_count));
        }
        Ok(&self.pages[id as usize])
    }

    pub(crate) fn flush_count(&self) -> u64 {
        self.flush_count
    }

    /// The page each write went to, in order.
    pub(crate) fn writes(&self) -> &[PageId] {
        &self.writes
    }

    /// The first page and page count of each call that wrote pages, in order.
    pub(crate) fn write_calls(&self) -> &[(PageId, usize)] {
        &self.calls
    }
}

#[cfg(test)]
impl PageDevice for MemoryPageDevice {
    fn page_count(&self) -> PageId {
        self.pages.len() as PageId
    }

    fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
        validate_buffer(destination.len())?;
        let page_count = self.page_count();
        if id >= page_count {
            return Err(out_of_range(id, page_count));
        }
        let page = &self.pages[id as usize];
        destination.copy_from_slice(page);
        Ok(())
    }

    fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
        validate_buffer(source.len())?;
        let page_count = self.page_count();
        if id == page_count && id < MAX_PAGE_COUNT {
            self.pages.push([0; PAGE_SIZE]);
        } else if id >= page_count {
            return Err(out_of_range(id, page_count));
        }
        let page = &mut self.pages[id as usize];
        page.copy_from_slice(source);
        self.writes.push(id);
        self.calls.push((id, 1));
        Ok(())
    }

    fn write_pages(&mut self, first: PageId, source: &[u8]) -> Result<()> {
        if source.is_empty() || !source.len().is_multiple_of(PAGE_SIZE) {
            return Err(device_error(format!(
                "Page device buffers must hold whole {PAGE_SIZE}-byte pages, not {} bytes",
                source.len()
            )));
        }
        for (offset, page) in source.chunks(PAGE_SIZE).enumerate() {
            self.write_page(first + offset as PageId, page)?;
            self.calls.pop();
        }
        self.calls.push((first, source.len() / PAGE_SIZE));
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_count += 1;
        Ok(())
    }
}

#[cfg(test)]
fn validate_buffer(length: usize) -> Result<()> {
    if length != PAGE_SIZE {
        return Err(device_error(format!(
            "Page device buffers must be exactly {PAGE_SIZE} bytes, not {length}"
        )));
    }
    Ok(())
}

#[cfg(test)]
fn out_of_range(id: PageId, page_count: PageId) -> EngineError {
    device_error(format!(
        "Page ID {id} is outside a device containing {page_count} pages"
    ))
}

#[cfg(test)]
fn device_error(message: impl Into<String>) -> EngineError {
    EngineError::new("PAGE_DEVICE_ERROR", message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_device_reads_writes_and_flushes() {
        let mut device = MemoryPageDevice::new(2).unwrap();
        let page = [7; PAGE_SIZE];
        device.write_page(1, &page).unwrap();
        let mut read = [0; PAGE_SIZE];
        device.read_page(1, &mut read).unwrap();
        assert_eq!(read, page);
        device.flush().unwrap();
        assert_eq!(device.flush_count(), 1);
    }

    #[test]
    fn memory_device_rejects_short_long_and_out_of_range_io() {
        let mut device = MemoryPageDevice::new(1).unwrap();
        assert_eq!(
            device
                .read_page(0, &mut [0; PAGE_SIZE - 1])
                .unwrap_err()
                .code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(
            device.write_page(0, &[0; PAGE_SIZE + 1]).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(
            device.read_page(1, &mut [0; PAGE_SIZE]).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(
            device
                .write_page(MAX_PAGE_COUNT, &[0; PAGE_SIZE])
                .unwrap_err()
                .code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(MemoryPageDevice::new(0).unwrap().page_count(), 0);
        assert_eq!(
            MemoryPageDevice::new(MAX_PAGE_COUNT + 1).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
    }

    #[test]
    fn memory_device_appends_dense_pages_without_eager_capacity_allocation() {
        let mut device = MemoryPageDevice::new(0).unwrap();
        device.write_page(0, &[1; PAGE_SIZE]).unwrap();
        device.write_page(1, &[2; PAGE_SIZE]).unwrap();
        assert_eq!(device.page_count(), 2);
        assert_eq!(device.page(1).unwrap(), &[2; PAGE_SIZE]);
        assert_eq!(
            device.write_page(3, &[3; PAGE_SIZE]).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(device.page_count(), 2);
    }

    #[derive(Debug)]
    struct FaultDevice {
        inner: MemoryPageDevice,
        fail_read: bool,
        fail_write: bool,
        fail_flush: bool,
    }

    impl PageDevice for FaultDevice {
        fn page_count(&self) -> PageId {
            self.inner.page_count()
        }

        fn read_page(&mut self, id: PageId, destination: &mut [u8]) -> Result<()> {
            if std::mem::take(&mut self.fail_read) {
                return Err(device_error("injected read failure"));
            }
            self.inner.read_page(id, destination)
        }

        fn write_page(&mut self, id: PageId, source: &[u8]) -> Result<()> {
            if std::mem::take(&mut self.fail_write) {
                return Err(device_error("injected write failure"));
            }
            self.inner.write_page(id, source)
        }

        fn flush(&mut self) -> Result<()> {
            if std::mem::take(&mut self.fail_flush) {
                return Err(device_error("injected flush failure"));
            }
            self.inner.flush()
        }
    }

    #[test]
    fn page_device_faults_propagate_without_mutating_buffers() {
        let mut device = FaultDevice {
            inner: MemoryPageDevice::new(1).unwrap(),
            fail_read: true,
            fail_write: true,
            fail_flush: true,
        };
        let mut destination = [9; PAGE_SIZE];
        assert_eq!(
            device.read_page(0, &mut destination).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(destination, [9; PAGE_SIZE]);
        assert_eq!(
            device.write_page(0, &[7; PAGE_SIZE]).unwrap_err().code,
            "PAGE_DEVICE_ERROR"
        );
        assert_eq!(device.inner.page(0).unwrap(), &[0; PAGE_SIZE]);
        assert_eq!(device.flush().unwrap_err().code, "PAGE_DEVICE_ERROR");
    }
}
