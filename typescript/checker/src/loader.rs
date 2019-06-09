use super::Checker;
use crate::{
    analyzer::{ExportInfo, ImportInfo},
    errors::Error,
    resolver::Resolve,
};
use fxhash::FxHashMap;
use std::{path::PathBuf, sync::Arc};
use swc_atoms::JsWord;

pub trait Load: Send + Sync {
    fn load(
        &self,
        base: Arc<PathBuf>,
        import: ImportInfo,
    ) -> Result<FxHashMap<JsWord, Arc<ExportInfo>>, Error>;
}

impl Load for Checker<'_> {
    fn load(
        &self,
        base: Arc<PathBuf>,
        import: ImportInfo,
    ) -> Result<FxHashMap<JsWord, Arc<ExportInfo>>, Error> {
        let mut result = FxHashMap::default();
        let mut errors = vec![];

        let path = self.resolver.resolve((*base).clone(), &import.src)?;
        let module = self.load_module(path);

        if import.all {
            result.extend(module.1.exports.clone())
        } else {
            for (sym, span) in import.items {
                if let Some(exported) = module.1.exports.get(&sym) {
                    result.insert(sym, exported.clone());
                } else {
                    errors.push((sym, span));
                }
            }
        }

        if errors.is_empty() {
            return Ok(result);
        }

        Err(Error::NoSuchExport { items: errors })
    }
}

impl<'a, T> Load for &'a T
where
    T: ?Sized + Load,
{
    fn load(
        &self,
        base: Arc<PathBuf>,
        import: ImportInfo,
    ) -> Result<FxHashMap<JsWord, Arc<ExportInfo>>, Error> {
        (**self).load(base, import)
    }
}