use crate::{
    AppContext, AsyncAppContext, Context, Effect, Entity, EntityId, EventEmitter, Model, Reference,
    Subscription, Task, WeakModel,
};
use derive_more::{Deref, DerefMut};
use futures::FutureExt;
use std::{
    any::{Any, TypeId},
    borrow::{Borrow, BorrowMut},
    future::Future,
};

#[derive(Deref, DerefMut)]
pub struct ModelContext<'a, T> {
    #[deref]
    #[deref_mut]
    app: Reference<'a, AppContext>,
    model_state: WeakModel<T>,
}

impl<'a, T: 'static> ModelContext<'a, T> {
    pub(crate) fn mutable(app: &'a mut AppContext, model_state: WeakModel<T>) -> Self {
        Self {
            app: Reference::Mutable(app),
            model_state,
        }
    }

    pub fn entity_id(&self) -> EntityId {
        self.model_state.entity_id
    }

    pub fn handle(&self) -> Model<T> {
        self.weak_model()
            .upgrade()
            .expect("The entity must be alive if we have a model context")
    }

    pub fn weak_model(&self) -> WeakModel<T> {
        self.model_state.clone()
    }

    pub fn observe<W, E>(
        &mut self,
        entity: &E,
        mut on_notify: impl FnMut(&mut T, E, &mut ModelContext<'_, T>) + 'static,
    ) -> Subscription
    where
        T: 'static,
        W: 'static,
        E: Entity<W>,
    {
        let this = self.weak_model();
        let entity_id = entity.entity_id();
        let handle = entity.downgrade();
        self.app.observers.insert(
            entity_id,
            Box::new(move |cx| {
                if let Some((this, handle)) = this.upgrade().zip(E::upgrade_from(&handle)) {
                    this.update(cx, |this, cx| on_notify(this, handle, cx));
                    true
                } else {
                    false
                }
            }),
        )
    }

    pub fn subscribe<T2, E>(
        &mut self,
        entity: &E,
        mut on_event: impl FnMut(&mut T, E, &T2::Event, &mut ModelContext<'_, T>) + 'static,
    ) -> Subscription
    where
        T: 'static,
        T2: 'static + EventEmitter,
        E: Entity<T2>,
    {
        let this = self.weak_model();
        let entity_id = entity.entity_id();
        let entity = entity.downgrade();
        self.app.event_listeners.insert(
            entity_id,
            Box::new(move |event, cx| {
                let event: &T2::Event = event.downcast_ref().expect("invalid event type");
                if let Some((this, handle)) = this.upgrade().zip(E::upgrade_from(&entity)) {
                    this.update(cx, |this, cx| on_event(this, handle, event, cx));
                    true
                } else {
                    false
                }
            }),
        )
    }

    pub fn on_release(
        &mut self,
        mut on_release: impl FnMut(&mut T, &mut AppContext) + 'static,
    ) -> Subscription
    where
        T: 'static,
    {
        self.app.release_listeners.insert(
            self.model_state.entity_id,
            Box::new(move |this, cx| {
                let this = this.downcast_mut().expect("invalid entity type");
                on_release(this, cx);
            }),
        )
    }

    pub fn observe_release<T2, E>(
        &mut self,
        entity: &E,
        mut on_release: impl FnMut(&mut T, &mut T2, &mut ModelContext<'_, T>) + 'static,
    ) -> Subscription
    where
        T: Any,
        T2: 'static,
        E: Entity<T2>,
    {
        let entity_id = entity.entity_id();
        let this = self.weak_model();
        self.app.release_listeners.insert(
            entity_id,
            Box::new(move |entity, cx| {
                let entity = entity.downcast_mut().expect("invalid entity type");
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| on_release(this, entity, cx));
                }
            }),
        )
    }

    pub fn observe_global<G: 'static>(
        &mut self,
        mut f: impl FnMut(&mut T, &mut ModelContext<'_, T>) + 'static,
    ) -> Subscription
    where
        T: 'static,
    {
        let handle = self.weak_model();
        self.global_observers.insert(
            TypeId::of::<G>(),
            Box::new(move |cx| handle.update(cx, |view, cx| f(view, cx)).is_ok()),
        )
    }

    pub fn on_app_quit<Fut>(
        &mut self,
        mut on_quit: impl FnMut(&mut T, &mut ModelContext<T>) -> Fut + 'static,
    ) -> Subscription
    where
        Fut: 'static + Future<Output = ()>,
        T: 'static,
    {
        let handle = self.weak_model();
        self.app.quit_observers.insert(
            (),
            Box::new(move |cx| {
                let future = handle.update(cx, |entity, cx| on_quit(entity, cx)).ok();
                async move {
                    if let Some(future) = future {
                        future.await;
                    }
                }
                .boxed_local()
            }),
        )
    }

    pub fn notify(&mut self) {
        if self
            .app
            .pending_notifications
            .insert(self.model_state.entity_id)
        {
            self.app.pending_effects.push_back(Effect::Notify {
                emitter: self.model_state.entity_id,
            });
        }
    }

    pub fn update_global<G, R>(&mut self, f: impl FnOnce(&mut G, &mut Self) -> R) -> R
    where
        G: 'static,
    {
        let mut global = self.app.lease_global::<G>();
        let result = f(&mut global, self);
        self.app.end_global_lease(global);
        result
    }

    pub fn spawn<Fut, R>(&self, f: impl FnOnce(WeakModel<T>, AsyncAppContext) -> Fut) -> Task<R>
    where
        T: 'static,
        Fut: Future<Output = R> + 'static,
        R: 'static,
    {
        let this = self.weak_model();
        self.app.spawn(|cx| f(this, cx))
    }
}

impl<'a, T> ModelContext<'a, T>
where
    T: EventEmitter,
{
    pub fn emit(&mut self, event: T::Event) {
        self.app.pending_effects.push_back(Effect::Emit {
            emitter: self.model_state.entity_id,
            event: Box::new(event),
        });
    }
}

impl<'a, T> Context for ModelContext<'a, T> {
    type ModelContext<'b, U> = ModelContext<'b, U>;
    type Result<U> = U;

    fn build_model<U: 'static>(
        &mut self,
        build_model: impl FnOnce(&mut Self::ModelContext<'_, U>) -> U,
    ) -> Model<U> {
        self.app.build_model(build_model)
    }

    fn update_model<U: 'static, R>(
        &mut self,
        handle: &Model<U>,
        update: impl FnOnce(&mut U, &mut Self::ModelContext<'_, U>) -> R,
    ) -> R {
        self.app.update_model(handle, update)
    }
}

impl<T> Borrow<AppContext> for ModelContext<'_, T> {
    fn borrow(&self) -> &AppContext {
        &self.app
    }
}

impl<T> BorrowMut<AppContext> for ModelContext<'_, T> {
    fn borrow_mut(&mut self) -> &mut AppContext {
        &mut self.app
    }
}