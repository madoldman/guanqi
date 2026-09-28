//! 主窗口顶栏的三个菜单：文件 / 对局 / 分析。
//!
//! 全部从原 `app.rs` 单体拆出，逐字节照搬：条目、置灰原因、悬停文案
//! 与执行体都不变（见 [`super::GuanqiApp`] 的字段文档）。侧栏只做
//! 信息展示，可改的设置一律规整进这三个菜单。

use super::GuanqiApp;
use super::play_flow::result_letter;
use super::PendingConfirm;
use crate::engine::Difficulty;
use crate::play;

impl GuanqiApp {
/// 「文件」菜单：棋谱的打开 / 另存，与研究副本的三个动作。
///
/// 副本动作做成子菜单（创建 / 切换 / 丢弃）而不是散在顶层：三个
/// 动作同属一个概念，且「切换 / 丢弃」在没开副本时不可用，收进
/// 子菜单后顶层只剩三条，与「对局」「分析」的条目数相当。
///
/// 置灰一律给原因（[`egui::Response::on_disabled_hover_text`]）：
/// 静默无效的菜单项比没有更糟——用户会以为点了没生效。
pub(super) fn file_menu(&mut self, ui: &mut egui::Ui) {
    ui.menu_button("文件", |ui| {
        let disabled_reason = self.portal_unavailable.clone();
        let mut entry = ui.add_enabled(
            disabled_reason.is_none(),
            egui::Button::new("打开棋谱…").shortcut_text("Ctrl+O"),
        );
        if let Some(reason) = disabled_reason.as_deref() {
            entry = entry.on_disabled_hover_text(reason.to_owned());
        }
        if entry.clicked() {
            self.open_file_dialog();
            // 菜单内的普通按钮不会自动收起菜单，显式关闭。
            ui.close();
        }
        let mut entry = ui.add_enabled(
            disabled_reason.is_none(),
            egui::Button::new("另存为…").shortcut_text("Ctrl+Shift+S"),
        );
        if let Some(reason) = disabled_reason.as_deref() {
            entry = entry.on_disabled_hover_text(reason.to_owned());
        }
        if entry.clicked() {
            self.save_file_dialog();
            ui.close();
        }
        // ---- 参照文件（阶段 22）：外部定式库 / 死活题谱的加载与卸载 ----
        // 与「打开棋谱」正交：不替换当前对局 / 文档，无内容守卫；
        // portal 不可用时同样置灰并说明原因。
        let mut entry = ui.add_enabled(
            disabled_reason.is_none(),
            egui::Button::new("打开参照文件…"),
        );
        if let Some(reason) = disabled_reason.as_deref() {
            entry = entry.on_disabled_hover_text(format!(
                "{reason}（参照文件用于局面匹配提示，不影响当前棋谱）"
            ));
        }
        if entry.clicked() {
            self.open_reference_dialog();
            ui.close();
        }
        let unload_disabled = if self.reference.is_none() {
            Some("当前没有已加载的参照文件")
        } else {
            None
        };
        let mut entry = ui.add_enabled(
            unload_disabled.is_none(),
            egui::Button::new("卸载参照"),
        );
        if let Some(reason) = unload_disabled {
            entry = entry.on_disabled_hover_text(reason);
        }
        if entry.clicked() {
            self.unload_reference();
            ui.close();
        }
        // ---- 导出复盘报告（阶段 21 #6）：未载入棋谱置灰并说明原因 ----
        // 复盘报告的内容是「当前棋谱的局后统计 + 差异手棋盘图」，新对局
        // 的统计属于「本盘」（与局后统计写回 SGF 的边界一致），故以
        // 「是否已载入棋谱」而非「线上有无手数」为准——空盘谱载入后也
        // 能导出（报告如实写「尚无分析数据」）。
        let report_disabled = if self.loaded.is_none() {
            Some("未载入棋谱，无复盘可导出")
        } else {
            None
        };
        let mut entry = ui.add_enabled(
            report_disabled.is_none(),
            egui::Button::new("导出复盘报告…"),
        );
        if let Some(reason) = report_disabled {
            entry = entry.on_disabled_hover_text(reason.to_owned());
        }
        if entry.clicked() {
            self.export_report_dialog();
            ui.close();
        }
        ui.separator();
        ui.menu_button("研究副本", |ui| {
            // 仅空盘置灰（原谱 / 任意副本里都可再开副本）。
            let copy_disabled = if self.loaded.is_none() {
                Some("未载入棋谱，无谱可复制")
            } else {
                None
            };
            let mut entry = ui.add_enabled(
                copy_disabled.is_none(),
                egui::Button::new("从当前手创建副本"),
            );
            if let Some(reason) = copy_disabled {
                entry = entry.on_disabled_hover_text(reason.to_owned());
            }
            if entry.clicked() {
                self.create_copy();
                ui.close();
            }
            ui.separator();
            // 「切换文档…」：展开各文档（原谱 + 各副本），当前项打勾。
            // 侧栏文档列表的**切换**保留（它本质是「看当前是哪份文档」
            // 的展示），这里给一个不依赖侧栏滚动位置的等价入口。
            if self.doc_entries().is_empty() {
                let entry = ui.add_enabled(false, egui::Button::new("切换文档…▸"));
                entry.on_disabled_hover_text("未载入棋谱");
            } else {
                ui.menu_button("切换文档…", |ui| {
                    for entry in self.doc_entries() {
                        if ui
                            .add(egui::Button::selectable(entry.active, &entry.name))
                            .clicked()
                        {
                            self.switch_doc(entry.number);
                            ui.close();
                        }
                    }
                });
            }
            // 「丢弃当前副本」：当前是原谱时置灰（无可丢）。有研究
            // 成果时 App 侧照旧弹确认框（确认机制与文案不变）。
            let drop_reason = match self.active_from_move {
                None => Some("当前是原谱，没有副本可丢弃".to_owned()),
                Some(_) => None,
            };
            let mut entry = ui.add_enabled(
                drop_reason.is_none(),
                egui::Button::new("丢弃当前副本"),
            );
            if let Some(reason) = drop_reason {
                entry = entry.on_disabled_hover_text(reason);
            }
            if entry.clicked() {
                // 编号即身份：丢弃语义与原侧栏行尾「丢弃」完全一致
                // （有研究成果先确认）。
                let number = self.active_number;
                let research = self.doc_research(number).unwrap_or(0);
                if research > 0 {
                    self.pending_confirm = PendingConfirm::DropCopy { number };
                } else {
                    self.drop_copy(number);
                }
                ui.close();
            }
        });
    });
}

/// 「对局」菜单：人机对弈开关、难度子菜单与终局动作。
///
/// 对弈类动作在**非对弈态**一律置灰并说明原因（原侧栏是靠「整张
/// 卡片只在 `play.mode` 时才渲染按钮」隐式隐藏，搬到菜单后不能靠
/// 隐藏——菜单项忽有忽无会让用户以为功能丢了，置灰 + 悬停原因才
/// 是「同一个动作、此刻不可用」的诚实表达）。
pub(super) fn play_menu(&mut self, ui: &mut egui::Ui) {
    ui.menu_button("对局", |ui| {
        if ui.button("新对局…").clicked() {
            self.new_game_open = true;
            ui.close();
        }
        ui.separator();
        // 人机对弈开关（复选）：与原侧栏同一个 `play.mode` 字段。
        // 勾选即刻生效（对局开始 = 从当前局面起人类执 `play.human`）。
        if ui.checkbox(&mut self.play.mode, "人机对弈").clicked() {
            ui.close();
        }
        // 难度子菜单：五档单选，当前项打勾；hover 给 visits 与
        // 预计等待（与原侧栏分段选择器的 hover 文案同一口径）。
        ui.menu_button("难度", |ui| {
            for &d in Difficulty::ALL.iter() {
                let selected = self.engine_cfg.play_difficulty == d;
                let response = ui
                    .add(egui::Button::selectable(selected, d.name()))
                    .on_hover_text(format!(
                        "{} visits，预计每手约 {} 秒",
                        d.visits(),
                        d.estimate_secs()
                    ));
                if response.clicked() && !selected {
                    self.set_difficulty(d);
                    ui.close();
                }
            }
        });
        ui.separator();
        // ---- 终局动作（认输 / 让引擎认输 / 弃着）----
        // 启用条件与原侧栏按钮**逐字照搬**：对弈中 && 未结束 &&
        // 轮到人类 && 游标在活子位置（让引擎认输另要求无望提示在）。
        let finished = self.play.finished(&self.board);
        let human_turn = self.board.to_play() == self.play.human;
        let live = self.board.cursor() == self.board.line_len();
        let playable = self.play.mode
            && !finished
            && human_turn
            && live
            && self.play.resigned.is_none()
            && self.play.timeout_loss.is_none();
        let idle_reason = "当前不是对局中（人机对弈未开启，或本局已结束）";
        for (text, enabled, reason) in [
            ("认输", playable, idle_reason),
            ("弃着", playable, idle_reason),
        ] {
            let entry = ui.add_enabled(enabled, egui::Button::new(text));
            if !enabled {
                entry.on_disabled_hover_text(reason);
            } else if entry.clicked() {
                match text {
                    "认输" => self.human_resign(),
                    "弃着" => self.human_pass(),
                    _ => {}
                }
                ui.close();
            }
        }
        // 「让引擎认输」：仅在引擎无望提示可用时启用（提示只给一次，
        // 与侧栏「让引擎认输」按钮的可见条件同源）。
        let hopeless = self.hopeless_now();
        let entry = ui.add_enabled(hopeless && playable, egui::Button::new("让引擎认输"));
        if !(hopeless && playable) {
            entry.on_disabled_hover_text(if hopeless {
                idle_reason
            } else {
                "引擎尚未认定自己无望（胜率 ≤ 5% 时才会出现该入口）"
            });
        } else if entry.clicked() {
            self.engine_resign();
            ui.close();
        }
        ui.separator();
        // 规则与时限：对局开始时确定、对局中不可改——对局中置灰并
        // 说明原因（不是「没有这个菜单项」，也不是静默无效）。
        let in_game = self.play.mode && !finished;
        let rules_line = self.menu_rules_line();
        let entry = ui.add_enabled(
            !in_game,
            egui::Button::new(format!("规则：{rules_line}")),
        );
        if in_game {
            entry.on_disabled_hover_text("规则在对局开始时确定，对局中不可改");
        }
        let time_line = self.play.clock.system.name().to_owned();
        let entry = ui.add_enabled(
            !in_game,
            egui::Button::new(format!("时限：{time_line}")),
        );
        if in_game {
            entry.on_disabled_hover_text("时限在对局开始时确定，对局中不可改");
        }
    });
}

/// 当前生效规则的中文名（「对局」菜单的只读行；与本局锁定 / 棋谱
/// RU 的解析链同源，见 `active_rules` 字段文档）。
fn menu_rules_line(&self) -> String {
    match self.active_rules.as_deref() {
        Some(wire) => crate::engine::Rules::from_wire(wire)
            .map_or_else(|| wire.to_owned(), |r| r.name().to_owned()),
        None => match self
            .loaded
            .as_ref()
            .and_then(|meta| meta.info.rules.as_deref())
        {
            Some(raw) => {
                let mapped = crate::engine::resolve_rules(None, Some(raw));
                crate::engine::Rules::from_wire(&mapped.rules)
                    .map_or_else(|| raw.to_owned(), |r| r.name().to_owned())
            }
            None => "中国".to_owned(),
        },
    }
}

/// 引擎方「无望」条件当前是否成立（「让引擎认输」入口的启用依据）。
/// 与**一次性的提示展示**（`should_show_hopeless`，消费「已提示」
/// 标记）刻意分开：入口可用性随局面持续成立，不因「提示已给过」
/// 而失效。
fn hopeless_now(&self) -> bool {
    play::engine_hopeless(&self.play, self.analysis.snapshot.as_ref())
}

/// 人类认输（菜单入口；与侧栏「认输」按钮执行体完全相同）。
fn human_resign(&mut self) {
    if self.play.mode && !self.play.finished(&self.board) {
        let loser = self.play.human;
        self.play.resigned = Some(loser);
        let winner = loser.opposite();
        let result = format!("{}+R", result_letter(winner));
        let text = format!(
            "{}认输：{}（{}）。对局已结束，可继续复盘浏览。",
            loser.name(),
            play::resign_text(loser),
            result
        );
        self.finish_game(result, text, None);
    }
}

/// 人类弃着（菜单入口；与侧栏「弃着」按钮执行体完全相同）。
fn human_pass(&mut self) {
    if self.play.mode && !self.play.finished(&self.board) {
        let mover = self.play.human;
        self.board.pass();
        self.play.clock.on_human_move(mover);
        if play::two_passes(&self.board) {
            self.finish_two_passes();
        }
    }
}

/// 「分析」菜单：整谱快扫（对话框）、限定选点、叠加层、候选显示、
/// 目数视角、底部面板。
///
/// 这些都是**分析口径的设置**，原先散在侧栏的四张卡片里；侧栏改为
/// 纯展示后统一归此。改的是同一份状态（[`overlay::Overlay`] /
/// [`AnalysisState`] 的门控与视角 / 限定选点），因此既有的偏好
/// 持久化（`sync_prefs` 每帧快照 + 防抖落盘）对菜单改动照旧生效。
pub(super) fn analyze_menu(&mut self, ui: &mut egui::Ui) {
    ui.menu_button("分析", |ui| {
        // ---- 整谱快扫：打开对话框（设置 + 预估 + 发起同屏）----
        if ui.button("整谱快扫…").clicked() {
            self.batch_scan_open = true;
            ui.close();
        }
        // 「取消快扫」：仅在快扫进行中可用（原侧栏进度区的取消按钮
        // 搬家入口）。侧栏进度卡是纯展示，取消动作归菜单；对话框里
        // 也有同一动作的按钮，两处执行体相同。
        let scanning = self.analysis.batch_progress().is_some();
        let entry = ui.add_enabled(scanning, egui::Button::new("取消快扫"));
        if !scanning {
            entry.on_disabled_hover_text("当前没有进行中的快扫");
        } else if entry.clicked() {
            self.analysis.cancel_batch();
            ui.close();
        }
        // ---- 另存写回：逐手注释开关（随 ui_prefs 持久化）----
        // 归「分析」菜单而非另存对话框：portal 另存对话框是系统原生
        // 窗口塞不进自绘控件，且注释是分析产物的写出口径，与分析设置
        // 同类。只影响下一次「另存为」，不改任何分析数据流。
        if ui
            .checkbox(&mut self.annotate_moves, "另存带逐手注释")
            .on_hover_text(
                "另存时把已分析出手（默认疑问手及以上；数据不足的手不注）\
                 的损失写成「【观棋 N】」注释块，合进各手节点的注释里——\
                 重复另存只更新块内容，原有注释文字不动。",
            )
            .changed()
        {
            self.mark_prefs_dirty();
        }
        ui.separator();
        // ---- 限定选点：区域开关 / 清除区域 / 清除排除列表 ----
        // 「清除排除列表」在原侧栏卡片里有独立按钮，搬家后必须仍
        // 可达（无排除项时置灰，说明「列表本来是空的」）。
        ui.menu_button("限定选点", |ui| {
            let limits = self.analysis.limits();
            let region_on = limits.has_region();
            // ---- 「排除首选看次优」便捷入口（KaTrain find_alternatives
            // 的同款语义，一键开关式）----
            // 「首选」取当前快照 order==0 的候选（引擎实时排序，排除后
            // 重查自动变成次优）；叠加而非替换：不清用户手动排除的其它手。
            // 区域模式下置灰并说明原因（allowMoves/avoidMoves 引擎实测
            // 互斥，与候选行「排除」按钮同一套文案口径）。
            let top = self
                .analysis
                .snapshot
                .as_ref()
                .and_then(|snap| snap.moves.iter().find(|info| info.order == 0))
                .and_then(|info| info.mv)
                .map(|at| (self.board.to_play(), at));
            // 入口已排除首选且列表里那条还在 ⇒ 按钮呈「撤销」态。
            // 用入口自己的 pin 记录判定，而不是只看列表非空——后者
            // 分不清「入口加的」与「用户手动加的」。
            let pinned = self.analysis.avoid_pin();
            let pinned_active = pinned.is_some_and(|pin| {
                self.analysis.limits().avoid.contains(&pin)
            });
            let (label, tip) = match (pinned_active, top) {
                (true, Some((player, at))) => (
                    format!("排除首选：撤销（已排除 {}）", at.to_gtp(self.board.size())),
                    format!(
                        "再点一次撤销刚才的排除（只移除 {} {}，\
                         手动排除的其它手不受影响）",
                        player.name(),
                        at.to_gtp(self.board.size())
                    ),
                ),
                (true, None) => (
                    "排除首选：撤销".to_owned(),
                    "再点一次撤销该入口的排除（首选已不在候选表，\
                     以记录的坐标为准）"
                        .to_owned(),
                ),
                (false, Some((player, at))) => (
                    format!("排除首选 {} 看次优", at.to_gtp(self.board.size())),
                    format!(
                        "把引擎当前首选 {} {} 加入排除并重查，\
                         看它不用时的最佳应手；再点一次撤销。\
                         与已排除的其它手叠加，不清空列表",
                        player.name(),
                        at.to_gtp(self.board.size())
                    ),
                ),
                (false, None) => (
                    "排除首选：暂无候选".to_owned(),
                    "引擎还没有当前局面的候选数据（或候选均为弃着），\
                     无法确定首选".to_owned(),
                ),
            };
            let has_top = pinned_active || top.is_some();
            let entry = ui.add_enabled(has_top && !region_on, egui::Button::new(label));
            // 三态提示：区域禁用给原因、无候选给原因、常态给功能说明。
            //（先算再挂：on_disabled_hover_text / on_hover_text 都消费
            // Response，三分支各自 move 会过不了借用检查。）
            let entry = if region_on {
                entry.on_disabled_hover_text(
                    "限定区域模式下排除不生效（引擎只允许区域或排除二者其一）；\
                     如需排除选点，请先关闭「限定区域」",
                )
            } else if !has_top {
                entry.on_disabled_hover_text(tip)
            } else {
                entry.on_hover_text(tip)
            };
            if has_top
                && !region_on
                && entry.clicked()
            {
                if pinned_active {
                    self.analysis.set_avoid_pin(None);
                } else if let Some((player, at)) = top {
                    self.analysis.set_avoid_pin(Some((player, at)));
                }
                ui.close();
            }
            ui.separator();
            if ui
                .add(egui::Button::selectable(
                    region_on,
                    if region_on {
                        "限定区域：开（在棋盘上拖框）"
                    } else {
                        "限定区域：关"
                    },
                ))
                .on_hover_text(if region_on {
                    "关闭区域并清除它（引擎恢复全盘选点）"
                } else {
                    "开启后在棋盘上拖框；引擎只考虑区域内的空点"
                })
                .clicked()
            {
                if region_on {
                    self.analysis.set_region(None);
                } else {
                    self.analysis.enable_region_mode();
                }
                ui.close();
            }
            let has_region = region_on;
            let entry = ui.add_enabled(has_region, egui::Button::new("清除区域"));
            if !has_region {
                entry.on_disabled_hover_text("当前没有限定区域");
            } else if entry.clicked() {
                self.analysis.set_region(None);
                ui.close();
            }
            let avoid_count = self.analysis.limits().avoid.len();
            let entry = ui.add_enabled(avoid_count > 0, egui::Button::new("清除排除列表"));
            if avoid_count == 0 {
                entry.on_disabled_hover_text("排除列表是空的");
            } else if entry.clicked() {
                self.analysis.clear_avoid();
                ui.close();
            }
            // 逐条移除（原侧栏排除行尾「移除」按钮的等价入口）：
            // 每行列出坐标，点击移除该条；侧栏「限定选点」卡展示的
            // 是同一份清单，两边坐标一一对应。清单先克隆（排除项
            // 至多几十条，量级极小），避免菜单闭包内同时持有只读
            // 迭代借用与点击后的可变借用。
            if avoid_count > 0 {
                ui.separator();
                ui.menu_button("移除排除项", |ui| {
                    let size = self.board.size();
                    let avoid: Vec<_> = self.analysis.limits().avoid.to_vec();
                    for (i, (player, at)) in avoid.iter().enumerate() {
                        if ui
                            .add(egui::Button::new(format!(
                                "{} {}",
                                player.name(),
                                at.to_gtp(size)
                            )))
                            .clicked()
                        {
                            self.analysis.remove_avoid(i);
                            ui.close();
                        }
                    }
                });
            }
            let has_any = has_region || avoid_count > 0;
            let entry = ui.add_enabled(has_any, egui::Button::new("清除全部限制"));
            if !has_any {
                entry.on_disabled_hover_text("没有区域也没有排除项");
            } else if entry.clicked() {
                self.analysis.clear_limits();
                ui.close();
            }
        });
        // ---- 叠加层：七项复选（与原侧栏同名同状态）----
        // 策略热度图 / 候选点领地两项是 opt-in 数据，翻转后必须
        // 经 AnalysisState 重发查询（走与侧栏一致的 want/sent 比对）。
        ui.menu_button("叠加层", |ui| {
            let overlay = &mut self.overlay;
            ui.checkbox(&mut overlay.show_candidates, "候选点圆圈");
            ui.checkbox(&mut overlay.show_heat, "局势热度图");
            let policy = ui
                .checkbox(&mut overlay.show_policy, "策略热度图")
                .on_hover_text(
                    "引擎还没搜索时的第一直觉（策略网络先验），\
                     不是搜索后的推荐——推荐看「候选点」。",
                );
            let moves_heat = ui
                .checkbox(&mut overlay.show_moves_heat, "候选点领地")
                .on_hover_text(
                    "开启后点击候选点定位时，热度图切换为「走这一手之后」的\
                     领地（紫色层，候选点级 ownership）。代价：每条报告按\
                     候选数增重，引擎需重查一次才生效；未聚焦候选点时仍\
                     显示当前局面。",
                );
            let pv_visits = ui
                .checkbox(&mut overlay.show_pv_visits, "候选悬停显示 PV 计算量")
                .on_hover_text(
                    "开启后候选点悬停文本显示主变各手的 visits，\
                     如「D4(120) → Q16(35) → …」——visits 越小 = \
                     引擎对这一步越不确定。代价：每条报告按候选数 × \
                     变长增加一小段整型数据（远小于「候选点领地」），\
                     引擎需重查一次才生效。",
                );
            ui.checkbox(&mut overlay.show_mistakes, "失误标注");
            ui.checkbox(&mut overlay.show_score_lead, "曲线叠加目差线");
            ui.checkbox(&mut overlay.show_mini_board, "小棋盘变化图");
            // opt-in 各项的翻转回传（与侧栏 card_overlay 同一条路径）。
            if policy.changed() {
                self.analysis.set_want_policy(overlay.show_policy);
            }
            if moves_heat.changed() {
                self.analysis.set_want_moves_ownership(overlay.show_moves_heat);
            }
            if pv_visits.changed() {
                self.analysis.set_want_pv_visits(overlay.show_pv_visits);
            }
        });
        // ---- 候选显示（门控）：三选一 + 延迟秒数 ----
        ui.menu_button("候选显示", |ui| {
            let gating = self.analysis.gating();
            let delay = match gating {
                crate::ui::analysis::CandidateGating::Delayed { secs } => secs,
                _ => self.engine_cfg.ui_prefs.gating_delay_secs,
            };
            for mode in [
                crate::ui::analysis::CandidateGating::Immediate,
                crate::ui::analysis::CandidateGating::Delayed { secs: delay },
                crate::ui::analysis::CandidateGating::Manual,
            ] {
                let selected =
                    std::mem::discriminant(&gating) == std::mem::discriminant(&mode);
                if ui
                    .add(egui::Button::selectable(selected, mode.name()))
                    .on_hover_text(match mode {
                        crate::ui::analysis::CandidateGating::Immediate => {
                            "收到报告即显示候选（默认）"
                        }
                        crate::ui::analysis::CandidateGating::Delayed { .. } => {
                            "引擎开始思考满 N 秒后才显示候选，避免浅层搜索结果误导"
                        }
                        crate::ui::analysis::CandidateGating::Manual => {
                            "按 F 显示候选；局面一变需重新按键"
                        }
                    })
                    .clicked()
                    && !selected
                {
                    self.analysis.set_gating(mode);
                    if mode != crate::ui::analysis::CandidateGating::Manual {
                        // 与 App 处理 SetGating 的同一条收尾：切回
                        // 立即 / 延迟时清掉手动确认残留。
                        self.manual_revealed = false;
                        self.manual_sig = None;
                    }
                }
            }
            // 延迟秒数：小数值控件（1..=30 秒）。
            if let crate::ui::analysis::CandidateGating::Delayed { secs } = gating {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.weak("延迟");
                    let mut edit = secs as i32;
                    if ui
                        .add(egui::DragValue::new(&mut edit).range(1..=30).suffix(" 秒"))
                        .changed()
                    {
                        self.analysis.set_gating(
                            crate::ui::analysis::CandidateGating::Delayed {
                                secs: edit.max(1) as u32,
                            },
                        );
                    }
                });
            }
        });
        // ---- 目数视角：两选一 ----
        ui.menu_button("目数视角", |ui| {
            let view = self.analysis.display_view();
            for mode in [
                crate::ui::analysis::DisplayView::Black,
                crate::ui::analysis::DisplayView::Alternating,
            ] {
                if ui
                    .add(egui::Button::selectable(view == mode, mode.name()))
                    .clicked()
                    && view != mode
                {
                    self.analysis.set_display_view(mode);
                }
            }
        });
        // ---- 底部面板：胜率曲线 / 棋谱树（小棋盘归叠加层，不重复）；
        // 思考过程卡片在侧栏（内容轻：一行读数 + 滑杆 + 几行概要，
        // 侧栏卡片比底部薄面板更合身），开关也放这里便于集中找到 ----
        ui.menu_button("面板", |ui| {
            ui.checkbox(&mut self.curve_open, "胜率曲线");
            ui.checkbox(&mut self.tree_open, "棋谱树");
            ui.checkbox(&mut self.thinking_open, "思考过程卡片").on_hover_text(
                "侧栏显示本次分析的稳定性读数与时间轴回放（首选翻转、何时稳定）",
            );
        });
    });
}
}
