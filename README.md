# Rpool

## Purpose

Rpool (Resource Pool joined) is a small Rust crate to handle abstract resource pooling. Rpool is a non-blocking implementation, providing state reset, externally immutable context, and opt-in automatic scaling (descaling is manual through the `reset` trait function).

## API

### Poolable

Any pooled data must implement the `rpool::Poolable` trait. Generally speaking, one would have a wrapper struct around a network stream or higher level resource, along with any necessary internal state specific to that resource.

```
pub trait Poolable<T>: Send + Sync {
    fn new(context: &T) -> Self;

    fn reset(&mut self) -> bool; // ran during return to the pool, must return true if resource is still valid.
}
```

The T type parameter is for the context type, use `()` if no inter-resource context is necessary. Internal mutability is safe through `Mutex` implementations or `std::atomic`.

### PoolScaleMode

`PoolScaleMode` is an exposed enum specifying one of two different scaling strategies that `rpool` can use.

* `Static { count: usize }`: Maintain a consistent number of resources at all times, and do not create more unless a resource fails to reset.
* `AutoScale { maximum: Option<usize>, initial: usize, chunk_size: usize }`: Start at `initial` resources allocated, increasing up to `maximum` or indefinitely in chunks of size `chunk_size`. If chunk_size is zero, the resource allocation is doubled during allocation. A reset resource in `AutoScale` is not automatically recreated immediately, but on demand.

### Pool

`Pool`s are constructed through `Pool::new::<ContextType, PoolableType>(scale_mode: PoolScaleMode, context: Y)`, which returns an `Arc<Pool<ContextType, PoolableType>>`.

The only exposed function on a `Pool` object is `get(&self) -> PoolGuard<ContextType, PoolableType>`.

`PoolGuard` transparently wraps `PoolableType` and returns the item into the pool upon being dropped.

## Examples

See `src/libs.rs`, `tests` module.