use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

// Android stuff
#[cfg(target_os = "android")]
use crate::android::{setup_class_loader, JAVAVM};
#[cfg(target_os = "android")]
use jni::AttachGuard;
#[cfg(target_os = "android")]
use log::{debug, info};
#[cfg(target_os = "android")]
use std::cell::RefCell;
#[cfg(target_os = "android")]
std::thread_local! {
    static JNI_ENV: RefCell<Option<AttachGuard<'static>>> = const { RefCell::new(None) };
}

#[cfg(not(target_os = "android"))]
pub(crate) fn create_runtime_and_block<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("intiface-thread-{}", id)
        })
        .build()
        .unwrap();
    runtime.block_on(future)
}

#[cfg(target_os = "android")]
pub(crate) fn create_runtime_and_block<F: Future>(future: F) -> F::Output {
    debug!("Call create_runtime from {:?}", std::thread::current());
    // Give time to accept permissions
    let vm = JAVAVM.get().unwrap();
    let env = vm.attach_current_thread().unwrap();

    let class_loader = setup_class_loader(&env);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("intiface-thread-{}", id)
        })
        .on_thread_stop(move || {
            info!("JNI Thread stopped");
            JNI_ENV.with(|f| *f.borrow_mut() = None);
        })
        .on_thread_start(move || {
            info!("JNI Thread started");
            // We now need to call the following code block via JNI calls. God help us.
            //
            //  java.lang.Thread.currentThread().setContextClassLoader(
            //    java.lang.ClassLoader.getSystemClassLoader()
            //  );

            let vm = JAVAVM.get().unwrap();
            let env = vm.attach_current_thread().unwrap();

            let thread = env
                .call_static_method(
                    "java/lang/Thread",
                    "currentThread",
                    "()Ljava/lang/Thread;",
                    &[],
                )
                .unwrap()
                .l()
                .unwrap();
            env.call_method(
                thread,
                "setContextClassLoader",
                "(Ljava/lang/ClassLoader;)V",
                &[class_loader.as_ref().unwrap().as_obj().into()],
            )
            .unwrap();
            JNI_ENV.with(|f| *f.borrow_mut() = Some(env));
        })
        .build()
        .unwrap();
    runtime.block_on(future)
}
