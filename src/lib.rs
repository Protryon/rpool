use std::sync::{ Arc, atomic::AtomicPtr, atomic::Ordering, atomic::AtomicUsize };
use std::ptr::null_mut;
use std::ops::{ Deref, DerefMut };
use std::fmt::{ Debug, Formatter, Result as FmtResult };

pub trait Poolable<T>: Send + Sync {
    fn new(context: &T) -> Self;

    fn reset(&mut self) -> bool; // true if still valid
}

pub struct PoolGuard<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> {
    // we are keeping the entire ItemNode here to prolong the lifetime outside of the `get` function.
    data: Option<Box<ItemNode<T>>>,
    pool: Arc<Pool<Y, T>>,
}

impl<Y: Send + Sync + Debug + 'static, T: Poolable<Y> + Debug + 'static> Debug for PoolGuard<Y, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let result = self.data.as_ref().map(|item| item.item.fmt(f));
        match result {
            Some(x) => x,
            None => write!(f, "expired pool guard"),
        }
    }
}

impl<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> Drop for PoolGuard<Y, T> {
    fn drop(&mut self) {
        self.pool.readd_node(self.data.take().unwrap().item);
    }
}

impl<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> Deref for PoolGuard<Y, T> {
    type Target = T;

    fn deref(&self) -> &T {
        return &self.data.as_ref().unwrap().item;
    }
}

impl<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> DerefMut for PoolGuard<Y, T> {

    fn deref_mut(&mut self) -> &mut Self::Target {
        return &mut self.data.as_mut().unwrap().item;
    }
}

pub enum PoolScaleMode {
    Static { count: usize },
    AutoScale { maximum: Option<usize>, initial: usize, chunk_size: usize }, // chunk_size = 0 for 2^n
}

struct ItemNode<T> {
    item: T,
    next: *mut ItemNode<T>,
}

pub struct Pool<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> {
    scale_mode: PoolScaleMode,
    items: AtomicPtr<ItemNode<T>>,
    count: AtomicUsize,
    capacity: AtomicUsize,
    context: Y,
}

impl<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> Drop for Pool<Y, T> {
    fn drop(&mut self) {
        // at this point, no guards should be alive as they have references to Pool
        let mut items = self.items.swap(null_mut(), Ordering::Relaxed);
        while !items.is_null() {
            let next_items = unsafe { items.as_ref().unwrap() }.next;
            drop(unsafe { Box::from_raw(items) });
            items = next_items;
        }
    }
}

impl<Y: Send + Sync + 'static, T: Poolable<Y> + 'static> Pool<Y, T> {
    pub fn new(scale_mode: PoolScaleMode, context: Y) -> Arc<Pool<Y, T>> {
        let pool = Arc::new(Pool {
            scale_mode,
            items: AtomicPtr::default(),
            count: AtomicUsize::new(0),
            capacity: AtomicUsize::new(0),
            context,
        });
        pool.init_pool();
        pool
    }

    fn init_pool(&self) {
        match &self.scale_mode {
            PoolScaleMode::Static { count } | PoolScaleMode::AutoScale { initial: count, .. } => {
                for _ in 0..*count {
                    self.capacity.fetch_add(1, Ordering::Acquire);
                    self.add_node(T::new(&self.context));
                }
            },
        }
    }

    fn readd_node(&self, mut item: T) {
        if !item.reset() {
            match self.scale_mode {
                PoolScaleMode::Static { .. } => {
                    self.add_node(T::new(&self.context));
                },
                _ => (),
            }
            return;
        }
        self.add_node(item);
    }

    fn add_node(&self, item: T) {
        let item_node = Box::into_raw(Box::new(ItemNode {
            item,
            next: null_mut(),
        }));
        self.count.fetch_add(1, Ordering::Acquire);
        loop {
            let present_node = self.items.load(Ordering::Acquire);
            unsafe { item_node.as_mut() }.unwrap().next = present_node;
            if self.items.compare_and_swap(present_node, item_node, Ordering::AcqRel) == present_node {
                break;
            }
        }
    }

    pub fn get(self: &Arc<Pool<Y, T>>) -> Option<PoolGuard<Y, T>> {
        loop {
            let present_node = self.items.load(Ordering::Acquire);
            if present_node.is_null() {
                match self.scale_mode {
                    PoolScaleMode::Static { .. } => {
                        // nothing we can do to get more right now
                    },
                    PoolScaleMode::AutoScale { maximum, chunk_size, .. } => {
                        let capacity = self.capacity.load(Ordering::Acquire);
                        if maximum.is_none() || capacity < maximum.unwrap() {
                            let new_capacity = capacity + if chunk_size == 0 {
                                if capacity == 0 {
                                    1
                                } else {
                                    capacity
                                }
                            } else {
                                chunk_size
                            };
                            let new_capacity = if maximum.is_some() && new_capacity > maximum.unwrap() {
                                maximum.unwrap()
                            } else {
                                new_capacity
                            };
                            while self.capacity.load(Ordering::Acquire) < new_capacity {
                                self.capacity.fetch_add(1, Ordering::Release);
                                self.add_node(T::new(&self.context));
                            }
                            continue;
                        } else {
                            // already at capacity
                        }
                    },
                }
                return None;
            }
            let present_node_ref = unsafe { present_node.as_mut() }.unwrap();
            if self.items.compare_and_swap(present_node, present_node_ref.next, Ordering::AcqRel) == present_node {
                let present_node_ref = unsafe { Box::from_raw(present_node) }; // take ownership / enforce we drop
                self.count.fetch_sub(1, Ordering::Release);
                let guard = PoolGuard {
                    data: Some(present_node_ref),
                    pool: self.clone(),
                };
                return Some(guard);
            }
        }
        
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::thread;

    #[derive(Debug)]
    struct TestContext {
        test: &'static str,
    }

    #[derive(Debug)]
    struct TestItem {
        test: String,
    }

    impl Poolable<TestContext> for TestItem {
        fn new(context: &TestContext) -> TestItem {
            TestItem {
                test: format!("{}_{}", context.test, "testing item"),
            }
        }

        fn reset(&mut self) -> bool {
            return true;
        }
    }

    #[test]
    fn test_creation() {
        let _: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::Static { count: 10 }, TestContext { test: "testing context" });
    }

    #[test]
    fn test_get() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::Static { count: 10 }, TestContext { test: "testing context" });
        for _ in 0..10 {
            let item = pool.get().expect("didn't find another item in pool");
            assert_eq!(item.test, "testing context_testing item");
            std::mem::forget(item);
        }
        assert!(pool.get().is_none());
    }

    #[test]
    fn test_get_refreshed() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::Static { count: 10 }, TestContext { test: "testing context" });
        for _ in 0..1000 {
            let item = pool.get().expect("didn't find another item in pool");
            assert_eq!(item.test, "testing context_testing item");
        }
        assert!(pool.get().is_some());
    }

    #[test]
    fn test_grow() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::AutoScale { maximum: None, initial: 0, chunk_size: 1 }, TestContext { test: "testing context" });
        for _ in 0..100 {
            let item = pool.get().expect("didn't find another item in pool");
            assert_eq!(item.test, "testing context_testing item");
            std::mem::forget(item);
        }
        assert_eq!(pool.capacity.load(Ordering::Relaxed), 100);
        assert!(pool.get().is_some());
    }

    #[test]
    fn test_grow_exponential() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::AutoScale { maximum: None, initial: 0, chunk_size: 0 }, TestContext { test: "testing context" });
        for _ in 0..100 {
            let item = pool.get().expect("didn't find another item in pool");
            assert_eq!(item.test, "testing context_testing item");
            std::mem::forget(item);
        }
        assert_eq!(pool.capacity.load(Ordering::Relaxed), 128);
        assert!(pool.get().is_some());
    }

    #[test]
    fn test_grow_capped() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::AutoScale { maximum: Some(10), initial: 0, chunk_size: 1 }, TestContext { test: "testing context" });
        for _ in 0..10 {
            let item = pool.get().expect("didn't find another item in pool");
            assert_eq!(item.test, "testing context_testing item");
            std::mem::forget(item);
        }
        assert_eq!(pool.capacity.load(Ordering::Relaxed), 10);
        assert!(pool.get().is_none());
    }

    #[test]
    fn test_race_readonly() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::Static { count: 1000 }, TestContext { test: "testing context" });
        let mut handles: Vec<thread::JoinHandle<_>> = vec![];
        for _ in 0..100 {
            let thread_pool = pool.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..10 {
                    let item = thread_pool.get().expect("didn't find another item in pool");
                    assert_eq!(item.test, "testing context_testing item");
                    std::mem::forget(item);
                }        
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(pool.capacity.load(Ordering::Relaxed), 1000);
        assert_eq!(pool.count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_race_read_return() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::Static { count: 1000 }, TestContext { test: "testing context" });
        let mut handles: Vec<thread::JoinHandle<_>> = vec![];
        for _ in 0..100 {
            let thread_pool = pool.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..10 {
                    let item = thread_pool.get().expect("didn't find another item in pool");
                    assert_eq!(item.test, "testing context_testing item");
                }        
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(pool.capacity.load(Ordering::Relaxed), 1000);
        assert_eq!(pool.count.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn test_race_read_grow() {
        let pool: Arc<Pool<TestContext, TestItem>> = Pool::new(PoolScaleMode::AutoScale { maximum: None, initial: 0, chunk_size: 1 }, TestContext { test: "testing context" });
        let mut handles: Vec<thread::JoinHandle<_>> = vec![];
        for _ in 0..1000 {
            let thread_pool = pool.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..110 {
                    let item = thread_pool.get().expect("didn't find another item in pool");
                    assert_eq!(item.test, "testing context_testing item");
                    std::mem::forget(item);
                }        
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(pool.count.load(Ordering::Relaxed), 0);
        assert!(pool.capacity.load(Ordering::Relaxed) >= 110000); // 1100+ due to racing creation vs counting, which is not a problem.
    }
}