use std::{borrow::Cow, fmt::Debug};

use rolldown_error::BuildError;
use rolldown_fs::FileSystem;

use super::{
  args::{HookBuildEndArgs, HookLoadArgs, HookResolveIdArgs, HookTransformArgs},
  context::{PluginContext, TransformPluginContext},
  output::{HookLoadOutput, HookResolveIdOutput},
};

pub type HookResolveIdReturn = Result<Option<HookResolveIdOutput>, BuildError>;
pub type HookTransformReturn = Result<Option<HookLoadOutput>, BuildError>;
pub type HookLoadReturn = Result<Option<HookLoadOutput>, BuildError>;
pub type HookNoopReturn = Result<(), BuildError>;

#[async_trait::async_trait]
pub trait Plugin<T: FileSystem + Default>: Debug + Send + Sync {
  fn name(&self) -> Cow<'static, str>;

  // The `option` hook consider call at node side.

  async fn build_start(&self, _ctx: &PluginContext<T>) -> HookNoopReturn {
    Ok(())
  }

  async fn resolve_id(
    &self,
    _ctx: &PluginContext<T>,
    _args: &HookResolveIdArgs,
  ) -> HookResolveIdReturn {
    Ok(None)
  }

  async fn load(&self, _ctx: &PluginContext<T>, _args: &HookLoadArgs) -> HookLoadReturn {
    Ok(None)
  }

  async fn transform(
    &self,
    _ctx: &TransformPluginContext<T, '_>,
    _args: &HookTransformArgs,
  ) -> HookTransformReturn {
    Ok(None)
  }

  async fn build_end(
    &self,
    _ctx: &PluginContext<T>,
    _args: Option<&HookBuildEndArgs>,
  ) -> HookNoopReturn {
    Ok(())
  }
}

pub type BoxPlugin<T> = Box<dyn Plugin<T>>;
