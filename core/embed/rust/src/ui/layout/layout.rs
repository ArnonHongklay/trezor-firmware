use core::{
    cell::RefCell,
    convert::{TryFrom, TryInto},
    time::Duration,
};

use crate::{
    error::Error,
    micropython::{
        gc::Gc,
        map::Map,
        obj::{Obj, ObjBase},
        qstr::Qstr,
        typ::Type,
    },
    ui::{
        component::{Component, Event, EventCtx, TimerToken},
        math::Point,
    },
    util,
};

/// In order to store any type of component in a layout, we need to access it
/// through an object-safe trait. `Component` itself is not object-safe because
/// of `Component::Msg` associated type. `ObjComponent` is a simple object-safe
/// wrapping trait that is implemented for all components where `Component::Msg`
/// can be converted to `Obj`.
pub trait ObjComponent {
    fn obj_event(&mut self, ctx: &mut EventCtx, event: Event) -> Obj;
    fn obj_paint_if_requested(&mut self);
}

impl<T> ObjComponent for T
where
    T: Component,
    T::Msg: Into<Obj>,
{
    fn obj_event(&mut self, ctx: &mut EventCtx, event: Event) -> Obj {
        self.event(ctx, event)
            .map_or_else(Obj::const_none, Into::into)
    }

    fn obj_paint_if_requested(&mut self) {
        self.paint_if_requested();
    }
}

/// `LayoutObj` is a GC-allocated object exported to MicroPython, with type
/// `LayoutObj::obj_type()`. It wraps a root component through the
/// `ObjComponent` trait.
#[repr(C)]
pub struct LayoutObj {
    base: ObjBase,
    inner: RefCell<LayoutObjInner>,
}

struct LayoutObjInner {
    root: Gc<dyn ObjComponent>,
    event_ctx: EventCtx,
    timer_fn: Obj,
}

impl LayoutObj {
    /// Create a new `LayoutObj`, wrapping a root component.
    pub fn new(root: impl ObjComponent + 'static) -> Gc<Self> {
        // SAFETY: We are coercing GC-allocated sized ptr into an unsized one.
        let root = unsafe { Gc::from_raw(Gc::into_raw(Gc::new(root)) as *mut dyn ObjComponent) };

        Gc::new(Self {
            base: Self::obj_type().to_base(),
            inner: RefCell::new(LayoutObjInner {
                root,
                event_ctx: EventCtx::new(),
                timer_fn: Obj::const_none(),
            }),
        })
    }

    /// Timer callback is expected to be a callable object of the following
    /// form: `def timer(token: int, deadline_in_ms: int)`.
    fn obj_set_timer_fn(&self, timer_fn: Obj) {
        self.inner.borrow_mut().timer_fn = timer_fn;
    }

    /// Run an event pass over the component tree. After the traversal, any
    /// pending timers are drained into `self.timer_callback`.
    fn obj_event(&self, event: Event) -> Obj {
        let inner = &mut *self.inner.borrow_mut();

        // SAFETY: `inner.root` is unique because of the `inner.borrow_mut()`.
        let msg = unsafe { Gc::as_mut(&mut inner.root) }.obj_event(&mut inner.event_ctx, event);

        // Drain any pending timers into the callback.
        while let Some((token, deadline)) = inner.event_ctx.pop_timer() {
            let token = token.try_into();
            let deadline = deadline.try_into();
            match (token, deadline) {
                (Ok(token), Ok(deadline)) => {
                    inner.timer_fn.call_with_n_args(&[token, deadline]);
                }
                _ => {
                    // Failed to convert token or deadline into `Obj`, skip.
                }
            }
        }

        msg
    }

    /// Run a paint pass over the component tree.
    fn obj_paint_if_requested(&self) {
        let mut inner = self.inner.borrow_mut();
        // SAFETY: `inner.root` is unique because of the `inner.borrow_mut()`.
        unsafe { Gc::as_mut(&mut inner.root) }.obj_paint_if_requested();
    }

    fn obj_type() -> &'static Type {
        static TYPE: Type = obj_type! {
            name: Qstr::MP_QSTR_Layout,
            locals: &obj_dict!(obj_map! {
                Qstr::MP_QSTR_set_timer_fn => obj_fn_2!(ui_layout_set_timer_fn).to_obj(),
                Qstr::MP_QSTR_touch_start => obj_fn_3!(ui_layout_touch_start).to_obj(),
                Qstr::MP_QSTR_touch_move => obj_fn_3!(ui_layout_touch_move).to_obj(),
                Qstr::MP_QSTR_touch_end => obj_fn_3!(ui_layout_touch_end).to_obj(),
                Qstr::MP_QSTR_timer => obj_fn_2!(ui_layout_timer).to_obj(),
                Qstr::MP_QSTR_paint => obj_fn_1!(ui_layout_paint).to_obj()
            }),
        };
        &TYPE
    }
}

impl Into<Obj> for Gc<LayoutObj> {
    fn into(self) -> Obj {
        // SAFETY:
        //  - We are GC-allocated.
        //  - We are `repr(C)`.
        //  - We have a `base` as the first field with the correct type.
        unsafe { Obj::from_ptr(Self::into_raw(self).cast()) }
    }
}

impl TryFrom<Obj> for Gc<LayoutObj> {
    type Error = Error;

    fn try_from(value: Obj) -> Result<Self, Self::Error> {
        if LayoutObj::obj_type().is_type_of(value) {
            // SAFETY: We assume that if `value` is an object pointer with the correct type,
            // it is always GC-allocated.
            let this = unsafe { Gc::from_raw(value.as_ptr().cast()) };
            Ok(this)
        } else {
            Err(Error::InvalidType)
        }
    }
}

impl TryFrom<Obj> for TimerToken {
    type Error = Error;

    fn try_from(value: Obj) -> Result<Self, Self::Error> {
        let raw: usize = value.try_into()?;
        let this = Self::from_raw(raw);
        Ok(this)
    }
}

impl From<TimerToken> for Obj {
    fn from(value: TimerToken) -> Self {
        value.into_raw().into()
    }
}

impl TryFrom<Duration> for Obj {
    type Error = Error;

    fn try_from(value: Duration) -> Result<Self, Self::Error> {
        let millis: usize = value.as_millis().try_into()?;
        Ok(millis.into())
    }
}

extern "C" fn ui_layout_set_timer_fn(this: Obj, timer_fn: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        this.obj_set_timer_fn(timer_fn);
        Ok(Obj::const_true())
    })
}

extern "C" fn ui_layout_touch_start(this: Obj, x: Obj, y: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        let event = Event::TouchStart(Point::new(x.try_into()?, y.try_into()?));
        let msg = this.obj_event(event);
        Ok(msg)
    })
}

extern "C" fn ui_layout_touch_move(this: Obj, x: Obj, y: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        let event = Event::TouchMove(Point::new(x.try_into()?, y.try_into()?));
        let msg = this.obj_event(event);
        Ok(msg)
    })
}

extern "C" fn ui_layout_touch_end(this: Obj, x: Obj, y: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        let event = Event::TouchEnd(Point::new(x.try_into()?, y.try_into()?));
        let msg = this.obj_event(event);
        Ok(msg)
    })
}

extern "C" fn ui_layout_timer(this: Obj, token: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        let event = Event::Timer(token.try_into()?);
        let msg = this.obj_event(event);
        Ok(msg)
    })
}

extern "C" fn ui_layout_paint(this: Obj) -> Obj {
    util::try_or_raise(|| {
        let this: Gc<LayoutObj> = this.try_into()?;
        this.obj_paint_if_requested();
        Ok(Obj::const_true())
    })
}
