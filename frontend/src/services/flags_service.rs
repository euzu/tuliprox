use crate::{error::Error, services::request_get_binary};
use shared::utils::FlagsLoader;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

#[derive(Clone, Default)]
pub struct FlagsService {
    loader: Rc<RefCell<Option<Rc<FlagsLoader>>>>,
    loading_started: Rc<Cell<bool>>,
}

impl FlagsService {
    pub fn new() -> Self { Self::default() }

    pub fn from_loader(loader: FlagsLoader) -> Self {
        Self { loader: Rc::new(RefCell::new(Some(Rc::new(loader)))), loading_started: Rc::new(Cell::new(true)) }
    }

    pub async fn ensure_loaded_from_assets(&self) -> Result<(), Error> {
        if self.is_loaded() || self.loading_started.replace(true) {
            return Ok(());
        }

        let bytes = match request_get_binary("assets/flags.dat").await {
            Ok(bytes) => bytes,
            Err(err) => {
                self.loading_started.set(false);
                return Err(err);
            }
        };
        let loader = match FlagsLoader::from_bytes(bytes) {
            Ok(loader) => loader,
            Err(_) => {
                self.loading_started.set(false);
                return Err(Error::DeserializeError);
            }
        };
        self.loader.replace(Some(Rc::new(loader)));
        Ok(())
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
