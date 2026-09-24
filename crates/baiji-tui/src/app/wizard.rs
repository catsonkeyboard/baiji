//! 配置向导：WizardStep / ConfigWizard 状态机与 App 的向导键处理、应用。

use super::{App, ChatLine, UiEvent};
use crate::settings::{self, RuntimeSettings};
use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc::UnboundedSender;

/// 输入步骤的按键拦截：Enter/Esc 不进输入框（确认/返回语义）
pub(crate) fn wizard_input_step_intercept(code: KeyCode) -> bool {
    matches!(code, KeyCode::Enter | KeyCode::Esc)
}

// ===== /config 配置向导 =====

/// 向导步骤（选厂商 → 选端点 → 输 Key → 选模型 → 生效）
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Vendor,
    Endpoint,
    Key,
    Model,
    ModelManual,
}

impl WizardStep {
    pub(crate) fn is_list(&self) -> bool {
        matches!(self, Self::Vendor | Self::Endpoint | Self::Model)
    }

    pub(crate) fn is_input(&self) -> bool {
        matches!(self, Self::Key | Self::ModelManual)
    }
}

pub struct ConfigWizard {
    pub step: WizardStep,
    pub vendor: String,
    pub endpoint: Option<String>,
    /// 向导输入的 Key（空 = 沿用现有配置）
    pub api_key: String,
    pub model: Option<String>,
    pub models: Vec<baiji_ai::ModelInfo>,
    pub models_error: Option<String>,
    pub fetching: bool,
    pub selected: usize,
}

impl ConfigWizard {
    pub(crate) fn new(vendor: &str, endpoint: Option<&str>) -> Self {
        Self {
            step: WizardStep::Vendor,
            vendor: vendor.to_string(),
            endpoint: endpoint.map(String::from),
            api_key: String::new(),
            model: None,
            models: Vec::new(),
            models_error: None,
            fetching: false,
            selected: 0,
        }
    }

    pub(crate) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn move_down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }
}

/// 向导输入步骤的按键路由：Enter/Esc 由向导拦截，其余（字符/退格/粘贴）
/// 落入正常输入处理进输入框。
impl App {
    pub(crate) async fn handle_wizard_key(&mut self, key: KeyEvent, ui_tx: &UnboundedSender<UiEvent>) -> bool {
        let is_list = match &self.wizard {
            Some(w) => w.step.is_list(),
            None => return false,
        };

        match key.code {
            KeyCode::Esc => {
                // 任意步骤直接关闭向导（重开成本低，避免多级回退）
                self.wizard = None;
            }
            KeyCode::Up | KeyCode::Char('k') if is_list => {
                if let Some(w) = &mut self.wizard {
                    w.move_up();
                }
            }
            KeyCode::Down | KeyCode::Char('j') if is_list => {
                let len = self.wizard_rows().map(|r| r.len()).unwrap_or(0);
                if let Some(w) = &mut self.wizard {
                    w.move_down(len);
                }
            }
            KeyCode::Enter => self.wizard_enter(ui_tx).await,
            _ => {}
        }
        false
    }

    /// 向导内 Enter：按步骤推进或生效
    pub(crate) async fn wizard_enter(&mut self, ui_tx: &UnboundedSender<UiEvent>) {
        let _ = ui_tx;
        // 先取只读快照，避免与下方可变借用冲突
        let Some(wizard) = self.wizard.as_ref() else {
            return;
        };
        let step = wizard.step.clone();
        let selected = wizard.selected;

        match step {
            WizardStep::Vendor => {
                let Some(vendors) = baiji_ai::all_vendors().get(selected) else {
                    return;
                };
                let id = vendors.id.to_string();
                let wizard = self.wizard.as_mut().unwrap();
                wizard.vendor = id;
                wizard.endpoint = None;
                wizard.step = WizardStep::Endpoint;
                wizard.selected = 0;
            }
            WizardStep::Endpoint => {
                let endpoint = self
                    .wizard
                    .as_ref()
                    .and_then(|w| baiji_ai::find_vendor(&w.vendor))
                    .and_then(|preset| {
                        preset.endpoint_names().get(selected).map(|n| {
                            if *n == "api" {
                                None
                            } else {
                                Some(n.to_string())
                            }
                        })
                    });
                let Some(endpoint) = endpoint else {
                    return;
                };
                let wizard = self.wizard.as_mut().unwrap();
                wizard.endpoint = endpoint;
                wizard.step = WizardStep::Key;
                wizard.selected = 0;
            }
            WizardStep::Key => {
                // 输入框内容为 Key；空 = 沿用现有
                let typed = self.input.trim().to_string();
                self.input.clear();
                {
                    let wizard = self.wizard.as_mut().unwrap();
                    if !typed.is_empty() {
                        wizard.api_key = typed;
                    }
                    wizard.step = WizardStep::Model;
                    wizard.selected = 0;
                    wizard.fetching = true;
                    wizard.models.clear();
                    wizard.models_error = None;
                }
                self.spawn_model_discovery();
            }
            WizardStep::Model => {
                // 最后一项固定为「手动输入」
                let rows_len = self.wizard_rows().map(|r| r.len()).unwrap_or(0);
                let manual = selected + 1 >= rows_len;
                if manual {
                    let wizard = self.wizard.as_mut().unwrap();
                    wizard.step = WizardStep::ModelManual;
                    wizard.selected = 0;
                    return;
                }
                let model = self
                    .wizard
                    .as_ref()
                    .and_then(|w| w.models.get(selected))
                    .map(|m| m.id.clone());
                if let Some(model) = model {
                    self.wizard.as_mut().unwrap().model = Some(model);
                    self.apply_wizard().await;
                }
            }
            WizardStep::ModelManual => {
                let typed = self.input.trim().to_string();
                self.input.clear();
                if typed.is_empty() {
                    return;
                }
                if let Some(w) = &mut self.wizard {
                    w.model = Some(typed);
                }
                self.apply_wizard().await;
            }
        }
    }

    /// 向导当前应使用的 Key：新输入优先；同厂商沿用现有；
    /// 切换了厂商则只取新厂商的环境变量——旧厂商的 Key 绝不外发给新厂商
    fn wizard_key(&self, wizard: &ConfigWizard) -> String {
        if !wizard.api_key.is_empty() {
            wizard.api_key.clone()
        } else if wizard.vendor == self.settings.vendor {
            self.settings.api_key.clone()
        } else {
            settings::key_for_vendor(&wizard.vendor)
        }
    }

    /// 派发模型发现任务（结果经通道回送事件循环）
    pub(crate) fn spawn_model_discovery(&mut self) {
        let Some(wizard) = &self.wizard else { return };
        let settings = RuntimeSettings {
            vendor: wizard.vendor.clone(),
            endpoint: wizard.endpoint.clone(),
            model: None,
            api_key: self.wizard_key(wizard),
            thinking: self.settings.thinking,
            external_agents: self.settings.external_agents.clone(),
        };
        let Some(tx) = &self.models_tx else { return };
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = settings::discover_models_async(&settings)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    /// 模型发现结果回填
    pub(crate) fn handle_models(&mut self, result: Result<Vec<baiji_ai::ModelInfo>, String>) {
        let Some(wizard) = &mut self.wizard else {
            return;
        };
        if wizard.step != WizardStep::Model {
            return;
        }
        wizard.fetching = false;
        match result {
            Ok(models) => {
                wizard.models = models;
                wizard.selected = 0;
            }
            Err(e) => wizard.models_error = Some(e),
        }
    }

    /// 保存 + 重建 Provider + 热切换 + 状态栏更新
    pub(crate) async fn apply_wizard(&mut self) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        let api_key = self.wizard_key(&wizard);
        // 新模型的上下文窗口（发现值优先，否则内置兜底）→ 压缩阈值
        let limits = wizard
            .model
            .as_deref()
            .map(|id| baiji_ai::model_limits(id, wizard.models.iter().find(|m| m.id == id)));
        // 只有用户显式输入的 Key 才落盘；展开后的明文（来自 $ENV）绝不回写
        let typed_key = wizard.api_key.clone();
        let key_update = if !typed_key.is_empty() {
            settings::KeyUpdate::Set(&typed_key)
        } else if wizard.vendor != self.settings.vendor {
            settings::KeyUpdate::Remove
        } else {
            settings::KeyUpdate::Keep
        };
        let new_settings = RuntimeSettings {
            vendor: wizard.vendor,
            endpoint: wizard.endpoint.filter(|e| e != "api"),
            model: wizard.model,
            api_key,
            // 向导不动思考级别与外部 agent：沿用当前值（save 会一并落盘）
            thinking: self.settings.thinking,
            external_agents: self.settings.external_agents.clone(),
        };

        if let Err(e) = settings::save(&self.config_path, &new_settings, key_update) {
            self.lines.push(ChatLine::System(crate::i18n::fill(
                &self.strings.cmd_config_save_fail_tpl,
                &[&e],
            )));
            self.scroll_to_bottom();
            return;
        }
        match settings::build_provider(&new_settings) {
            Ok(provider) => {
                let hint = settings::status_hint(&new_settings);
                {
                    let mut harness = self.harness.lock().await;
                    harness.swap_provider(provider);
                    if let Some(limits) = limits {
                        harness.set_context_window(limits.context_length);
                    }
                }
                self.status_hint = hint;
                let endpoint = new_settings.endpoint.as_deref().unwrap_or("api");
                let model_label = new_settings.model.clone().unwrap_or_else(|| "?".into());
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_applied_tpl,
                    &[&new_settings.vendor, &endpoint.to_string(), &model_label],
                )));
                self.settings = new_settings;
            }
            Err(e) => {
                self.lines.push(ChatLine::System(crate::i18n::fill(
                    &self.strings.cmd_swap_fail_tpl,
                    &[&e],
                )));
                self.settings = new_settings;
            }
        }
        self.scroll_to_bottom();
    }

}
