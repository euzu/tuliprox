use crate::{error::Error, services::request_get_binary};
use futures_signals::signal::{Mutable, SignalExt};
use log::error;
use shared::utils::FlagsLoader;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagsLoadState {
    Loaded,
    InProgress,
}

#[derive(Clone)]
pub struct FlagsService {
    loader: Rc<RefCell<Option<Rc<FlagsLoader>>>>,
    loading_started: Rc<Cell<bool>>,
    loaded_channel: Rc<Mutable<bool>>,
}

impl Default for FlagsService {
    fn default() -> Self {
        Self {
            loader: Rc::new(RefCell::new(None)),
            loading_started: Rc::new(Cell::new(false)),
            loaded_channel: Rc::new(Mutable::new(false)),
        }
    }
}

impl FlagsService {
    pub fn new() -> Self { Self::default() }

    pub fn from_loader(loader: FlagsLoader) -> Self {
        Self {
            loader: Rc::new(RefCell::new(Some(Rc::new(loader)))),
            loading_started: Rc::new(Cell::new(true)),
            loaded_channel: Rc::new(Mutable::new(true)),
        }
    }

    pub async fn loaded_subscribe<F, U>(&self, callback: &mut F)
    where
        U: Future<Output = ()>,
        F: FnMut(bool) -> U,
    {
        let fut = self.loaded_channel.signal_cloned().for_each(callback);
        fut.await
    }

    pub async fn ensure_loaded_from_assets(&self) -> Result<FlagsLoadState, Error> {
        if self.is_loaded() {
            return Ok(FlagsLoadState::Loaded);
        }
        if self.loading_started.replace(true) {
            return Ok(FlagsLoadState::InProgress);
        }

        let bytes = match request_get_binary("assets/flags.dat").await {
            Ok(bytes) => bytes,
            Err(err) => {
                self.loading_started.set(false);
                self.loaded_channel.set(false);
                return Err(err);
            }
        };
        let loader = match FlagsLoader::from_bytes(bytes) {
            Ok(loader) => loader,
            Err(err) => {
                self.loading_started.set(false);
                self.loaded_channel.set(false);
                error!("Failed to parse flags.dat: {err}");
                return Err(Error::DeserializeError);
            }
        };
        self.loader.replace(Some(Rc::new(loader)));
        self.loaded_channel.set(true);
        Ok(FlagsLoadState::Loaded)
    }

    pub fn get_flag(&self, country_code: &str) -> Option<String> {
        self.loader.borrow().as_ref().and_then(|loader| loader.get_flag(country_code))
    }

    pub fn has_flag(&self, country_code: &str) -> bool {
        self.loader.borrow().as_ref().is_some_and(|loader| loader.has_flag(country_code))
    }

    pub fn count(&self) -> usize { self.loader.borrow().as_ref().map_or(0, |loader| loader.count()) }

    pub fn is_loaded(&self) -> bool { self.loader.borrow().is_some() }
}

impl PartialEq for FlagsService {
    fn eq(&self, other: &Self) -> bool { Rc::ptr_eq(&self.loader, &other.loader) }
}

impl Eq for FlagsService {}
