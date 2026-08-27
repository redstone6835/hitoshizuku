#![no_std]
#![no_main]

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, Ordering};

use elm::{
    ElmModule, HookError, HookResult, KernelMixinContext, LifecycleContext, MigrationContext,
    MigrationExportResult,
};

use allocator as _;
use general as _;

#[cfg(not(feature = "replacement"))]
const IMAGE_NAME: &str = "v1";
#[cfg(feature = "replacement")]
const IMAGE_NAME: &str = "v2";

static HEAD_REPORTED: AtomicBool = AtomicBool::new(false);
static ARGUMENT_REPORTED: AtomicBool = AtomicBool::new(false);
static OVERWRITE_REPORTED: AtomicBool = AtomicBool::new(false);
static RETURN_REPORTED: AtomicBool = AtomicBool::new(false);

struct KernelMixinTest;

#[elm::module]
impl ElmModule for KernelMixinTest {
    fn create(_context: &LifecycleContext) -> Result<Self, HookError> {
        Ok(Self)
    }

    fn initialize(&mut self, _context: &LifecycleContext) -> HookResult {
        reset_reports();
        report_image("initialize")
    }

    fn finalize(&mut self, _context: &LifecycleContext) -> HookResult {
        report_image("finalize")
    }

    fn quiesce(&mut self, _context: &LifecycleContext) -> HookResult {
        report_image("quiesce")
    }

    fn pause(&mut self, _context: &LifecycleContext) -> HookResult {
        reset_reports();
        report_image("pause")
    }

    fn resume(&mut self, _context: &LifecycleContext) -> HookResult {
        reset_reports();
        report_image("resume")
    }

    fn migrate_export(
        &self,
        _context: &MigrationContext,
        output: &mut [u8],
    ) -> MigrationExportResult {
        let Some(slot) = output.first_mut() else {
            return Err(HookError::new(-1));
        };
        *slot = 1;
        report_image("migrate-export")?;
        Ok(1)
    }

    fn migrate_import(&mut self, _context: &MigrationContext, input: &[u8]) -> HookResult {
        if input != [1] {
            return Err(HookError::new(-1));
        }
        #[cfg(feature = "reject-migration")]
        {
            report_image("migrate-import-rejected")?;
            return Err(HookError::new(-1));
        }
        #[cfg(not(feature = "reject-migration"))]
        {
            reset_reports();
            report_image("migrate-import")
        }
    }

    fn migrate_abort(&mut self, _context: &MigrationContext, input: &[u8]) -> HookResult {
        if input != [1] {
            return Err(HookError::new(-1));
        }
        reset_reports();
        report_image("migrate-abort")
    }
}

#[elm::mixin(target = "allocator")]
impl KernelMixinTest {
    #[elm::inject(method = "GlobalAlloc.alloc", at = "head", priority = 300)]
    #[cfg_attr(feature = "elm-integrated", allow(dead_code))]
    fn trace_alloc_head(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
        if context.argument_count() != 2
            || context.argument::<Layout>(1).is_none()
            || context.result_ready()
        {
            return Err(HookError::new(-1));
        }
        report_once(&HEAD_REPORTED, "alloc-head")
    }

    #[elm::modify_arg(method = "GlobalAlloc.alloc", priority = 200)]
    #[cfg_attr(feature = "elm-integrated", allow(dead_code))]
    fn validate_alloc_argument(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
        let Some(layout) = context.argument_mut::<Layout>(1) else {
            return Err(HookError::new(-1));
        };
        *layout = *layout;
        report_once(&ARGUMENT_REPORTED, "alloc-argument")
    }

    #[elm::overwrite(method = "GlobalAlloc.alloc", priority = 100)]
    #[cfg_attr(feature = "elm-integrated", allow(dead_code))]
    fn wrap_alloc(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
        report_once(&OVERWRITE_REPORTED, "alloc-overwrite")?;
        context.proceed()
    }

    #[elm::modify_return(method = "GlobalAlloc.alloc", priority = 100)]
    #[cfg_attr(feature = "elm-integrated", allow(dead_code))]
    fn validate_alloc_return(&self, context: &mut KernelMixinContext<'_>) -> HookResult {
        if context.result::<*mut u8>().is_none() {
            return Err(HookError::new(-1));
        }
        report_once(&RETURN_REPORTED, "alloc-return")
    }
}

fn reset_reports() {
    HEAD_REPORTED.store(false, Ordering::Release);
    ARGUMENT_REPORTED.store(false, Ordering::Release);
    OVERWRITE_REPORTED.store(false, Ordering::Release);
    RETURN_REPORTED.store(false, Ordering::Release);
}

#[cfg_attr(feature = "elm-integrated", allow(dead_code))]
fn report_once(flag: &AtomicBool, event: &str) -> HookResult {
    if !flag.swap(true, Ordering::AcqRel) {
        report(event)?;
    }
    Ok(())
}

fn report_image(event: &str) -> HookResult {
    match (IMAGE_NAME, event) {
        ("v1", "initialize") => report("v1 initialize"),
        ("v1", "finalize") => report("v1 finalize"),
        ("v1", "quiesce") => report("v1 quiesce"),
        ("v1", "pause") => report("v1 pause"),
        ("v1", "resume") => report("v1 resume"),
        ("v1", "migrate-export") => report("v1 migrate-export"),
        ("v1", "migrate-import") => report("v1 migrate-import"),
        ("v1", "migrate-import-rejected") => report("v1 migrate-import-rejected"),
        ("v1", "migrate-abort") => report("v1 migrate-abort"),
        ("v2", "initialize") => report("v2 initialize"),
        ("v2", "finalize") => report("v2 finalize"),
        ("v2", "quiesce") => report("v2 quiesce"),
        ("v2", "pause") => report("v2 pause"),
        ("v2", "resume") => report("v2 resume"),
        ("v2", "migrate-export") => report("v2 migrate-export"),
        ("v2", "migrate-import") => report("v2 migrate-import"),
        ("v2", "migrate-import-rejected") => report("v2 migrate-import-rejected"),
        ("v2", "migrate-abort") => report("v2 migrate-abort"),
        _ => Err(HookError::new(-1)),
    }
}

fn report(event: &str) -> HookResult {
    elm::runtime::log(6, event).map_err(|_| HookError::new(-1))
}

#[cfg(not(feature = "elm-integrated"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    elm::runtime::abort_panic()
}
