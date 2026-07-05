//! Schema-tree sidebar: model types, the pure build/sync functions, and the
//! `Workspace` methods that load schema data and sync it into the tree widget.

use super::*;

/// All sidebar schema-tree state: the loaded model, the widget's retained
/// items/state, expansion, filter, and lazy-load bookkeeping.
pub(super) struct SchemaTree {
    /// Loaded schema nodes (the model).
    pub(super) schemas: Vec<SchemaNode>,
    pub(super) state: Entity<TreeState>,
    /// Retained roots; their `Rc` expansion state is shared with the widget.
    pub(super) items: Vec<TreeItem>,
    pub(super) meta: NodeMetaMap,
    /// Source of truth for which nodes are expanded, keyed by node id.
    pub(super) expanded: HashSet<SharedString>,
    /// Last selected node id, restored across `set_items` (which wipes it).
    pub(super) sel: Option<SharedString>,
    /// Sidebar filter text (trimmed); non-empty = filter mode.
    pub(super) filter: String,
    pub(super) filter_input: Entity<InputState>,
    /// Relation id to select + scroll to once its schema's relations load.
    pub(super) pending_reveal: Option<SharedString>,
    /// In-flight lazy-load guard (schema + relation ids).
    pub(super) loads: HashSet<SharedString>,
    /// Clear the filter input on the next render (needs `&mut Window`; set
    /// from window-less contexts like `switch_connection`).
    pub(super) pending_clear_filter: bool,
}

impl SchemaTree {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let state = cx.new(|cx| TreeState::new(cx));
        // Fires on every widget-side select/toggle (they all notify).
        cx.observe(&state, Workspace::on_tree_notify).detach();

        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables…"));
        cx.subscribe(&filter_input, |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let v = input.read(cx).value().trim().to_string();
                if v != this.tree.filter {
                    this.tree.filter = v;
                    this.sync_tree(cx);
                }
            }
        })
        .detach();

        Self {
            schemas: Vec::new(),
            state,
            items: Vec::new(),
            meta: NodeMetaMap::new(),
            expanded: HashSet::new(),
            sel: None,
            filter: String::new(),
            filter_input,
            pending_reveal: None,
            loads: HashSet::new(),
            pending_clear_filter: false,
        }
    }

    /// Drop all tree state (connection switch — the old schemas are invalid).
    pub(super) fn reset(&mut self, cx: &mut Context<Workspace>) {
        self.schemas.clear();
        self.items.clear();
        self.meta.clear();
        self.expanded.clear();
        self.sel = None;
        self.pending_reveal = None;
        self.loads.clear();
        self.filter.clear();
        self.pending_clear_filter = true;
        self.state.update(cx, |ts, cx| ts.set_items(Vec::<TreeItem>::new(), cx));
    }
}

pub(super) struct SchemaNode {
    pub(super) name: String,
    pub(super) relations: Option<Vec<RelNode>>,
    /// Sequences + functions, loaded alongside relations on first expand.
    pub(super) objects: Option<SchemaObjects>,
}

pub(super) struct RelNode {
    pub(super) name: String,
    pub(super) kind: RelationKind,
    /// Columns + indexes + constraints, loaded when this relation is expanded.
    pub(super) detail: Option<RelationDetail>,
}

// ---- schema tree (gpui-component Tree widget) ----------------------------
//
// The sidebar is a virtualized `gpui_component::tree` fed from `Workspace.tree.schemas`
// via `build_tree`. Expansion lives in `Workspace.tree.expanded` (node-id keyed);
// widget-driven toggles flow back through the shared `Rc<RefCell<…>>` state
// inside each retained `TreeItem` (its `Clone` shares that state) and are
// synced by `sync_expansion` from the tree-state observer.

/// Separator for tree-node ids: can't appear in SQL identifiers.
const ID_SEP: &str = "\u{1}";
/// Schema-node id prefix (`sch<SEP>name`), used to spot transient filter-expansion.
const SCH_PREFIX: &str = "sch\u{1}";

pub(super) fn node_id(parts: &[&str]) -> SharedString {
    parts.join(ID_SEP).into()
}

/// What a tree node IS, keyed by node id — render/click/menu handlers look
/// things up here instead of parsing ids.
#[derive(Clone)]
pub(super) enum NodeMeta {
    Db,
    Schema { name: String },
    Rel { schema: String, name: String, kind: RelationKind },
    /// Small-caps section header (COLUMNS / INDEXES / CONSTRAINTS / …).
    Group,
    /// Column/index/constraint/sequence/function leaf: name + dim meta text.
    Leaf { name: String, meta: String },
    /// "loading…" / "(empty)" filler row (disabled in the widget).
    Placeholder,
}

pub(super) type NodeMetaMap = HashMap<SharedString, NodeMeta>;

/// A disabled filler child. Also makes the parent a folder (`is_folder()` is
/// `children.len() > 0`) so the widget lets the user expand it before the
/// lazy load lands.
pub(super) fn placeholder_item(parent: &SharedString, label: &'static str, meta: &mut NodeMetaMap) -> TreeItem {
    let id: SharedString = format!("load{ID_SEP}{parent}").into();
    meta.insert(id.clone(), NodeMeta::Placeholder);
    TreeItem::new(id, label).disabled(true)
}

pub(super) fn rel_tree_item(
    schema: &str,
    rel: &RelNode,
    expanded: &HashSet<SharedString>,
    meta: &mut NodeMetaMap,
) -> TreeItem {
    let rid = node_id(&["rel", schema, &rel.name]);
    meta.insert(
        rid.clone(),
        NodeMeta::Rel { schema: schema.to_string(), name: rel.name.clone(), kind: rel.kind },
    );
    let mut item = TreeItem::new(rid.clone(), rel.name.clone());
    // Only tables expand (columns are shown for tables only); other kinds are
    // leaves that open on click.
    if rel.kind != RelationKind::Table {
        return item;
    }
    match &rel.detail {
        None => item = item.child(placeholder_item(&rid, "loading…", meta)),
        Some(d) => {
            let group = |tag: &str, label_id: &mut NodeMetaMap| -> SharedString {
                let gid = node_id(&[tag, schema, &rel.name]);
                label_id.insert(gid.clone(), NodeMeta::Group);
                gid
            };

            let gid = group("cols", meta);
            let mut leaves = Vec::new();
            for c0 in &d.columns {
                let id = node_id(&["col", schema, &rel.name, &c0.name]);
                let mut ty = c0.type_name.clone();
                if c0.is_primary_key {
                    ty.push_str("  PK");
                } else if !c0.nullable {
                    ty.push_str("  NOT NULL");
                }
                meta.insert(id.clone(), NodeMeta::Leaf { name: c0.name.clone(), meta: ty });
                leaves.push(TreeItem::new(id, c0.name.clone()));
            }
            if leaves.is_empty() {
                leaves.push(placeholder_item(&gid, "(none)", meta));
            }
            item = item.child(
                TreeItem::new(gid.clone(), "COLUMNS")
                    .children(leaves)
                    .expanded(expanded.contains(&gid)),
            );

            if !d.indexes.is_empty() {
                let gid = group("idxs", meta);
                let mut leaves = Vec::new();
                for ix in &d.indexes {
                    let id = node_id(&["idx", schema, &rel.name, &ix.name]);
                    let tag = if ix.is_primary {
                        "PRIMARY"
                    } else if ix.is_unique {
                        "UNIQUE"
                    } else {
                        ""
                    };
                    meta.insert(
                        id.clone(),
                        NodeMeta::Leaf { name: ix.name.clone(), meta: tag.to_string() },
                    );
                    leaves.push(TreeItem::new(id, ix.name.clone()));
                }
                item = item.child(
                    TreeItem::new(gid.clone(), "INDEXES")
                        .children(leaves)
                        .expanded(expanded.contains(&gid)),
                );
            }

            if !d.constraints.is_empty() {
                let gid = group("cons", meta);
                let mut leaves = Vec::new();
                for con in &d.constraints {
                    let id = node_id(&["con", schema, &rel.name, &con.name]);
                    let kind = match con.kind {
                        'p' => "PRIMARY KEY",
                        'f' => "FOREIGN KEY",
                        'u' => "UNIQUE",
                        'c' => "CHECK",
                        'x' => "EXCLUDE",
                        _ => "",
                    };
                    meta.insert(
                        id.clone(),
                        NodeMeta::Leaf { name: con.name.clone(), meta: kind.to_string() },
                    );
                    leaves.push(TreeItem::new(id, con.name.clone()));
                }
                item = item.child(
                    TreeItem::new(gid.clone(), "CONSTRAINTS")
                        .children(leaves)
                        .expanded(expanded.contains(&gid)),
                );
            }
        }
    }
    item.expanded(expanded.contains(&rid))
}

/// Build the widget's item hierarchy (db → schemas → TABLES/SEQUENCES/
/// FUNCTIONS groups → relations → leaves) from the model. Pure: expansion
/// comes from `expanded`, and a
/// non-empty `filter` keeps only matching relations, force-expanding db +
/// schemas transiently (never written back to `expanded`).
pub(super) fn build_tree(
    dbname: Option<&str>,
    tree: &[SchemaNode],
    expanded: &HashSet<SharedString>,
    filter: &str,
) -> (Vec<TreeItem>, NodeMetaMap) {
    let mut meta = NodeMetaMap::new();
    let Some(dbname) = dbname else {
        return (Vec::new(), meta);
    };
    let filter = filter.trim().to_lowercase();
    let filtering = !filter.is_empty();

    let mut schema_items = Vec::new();
    for node in tree {
        let sid = node_id(&["sch", &node.name]);
        let mut kids: Vec<TreeItem> = Vec::new();
        let mut matches = 0usize;
        match &node.relations {
            None => kids.push(placeholder_item(&sid, "loading…", &mut meta)),
            Some(rels) => {
                let gid = node_id(&["tbls", &node.name]);
                meta.insert(gid.clone(), NodeMeta::Group);
                let mut rel_items = Vec::new();
                for rel in rels {
                    if filtering && !rel.name.to_lowercase().contains(&filter) {
                        continue;
                    }
                    matches += 1;
                    rel_items.push(rel_tree_item(&node.name, rel, expanded, &mut meta));
                }
                if rel_items.is_empty() {
                    rel_items.push(placeholder_item(&gid, "(empty)", &mut meta));
                }
                kids.push(
                    TreeItem::new(gid.clone(), "TABLES")
                        .children(rel_items)
                        .expanded(filtering || expanded.contains(&gid)),
                );
                if !filtering {
                    if let Some(objs) = &node.objects {
                        let obj_group = |tag: &str,
                                             label: &'static str,
                                             leaf_tag: &str,
                                             names: &[String],
                                             meta: &mut NodeMetaMap|
                         -> Option<TreeItem> {
                            if names.is_empty() {
                                return None;
                            }
                            let gid = node_id(&[tag, &node.name]);
                            meta.insert(gid.clone(), NodeMeta::Group);
                            let mut leaves = Vec::new();
                            for name in names {
                                let id = node_id(&[leaf_tag, &node.name, name]);
                                meta.insert(
                                    id.clone(),
                                    NodeMeta::Leaf { name: name.clone(), meta: String::new() },
                                );
                                leaves.push(TreeItem::new(id, name.clone()));
                            }
                            Some(
                                TreeItem::new(gid.clone(), label)
                                    .children(leaves)
                                    .expanded(expanded.contains(&gid)),
                            )
                        };
                        kids.extend(obj_group("seqs", "SEQUENCES", "seq", &objs.sequences, &mut meta));
                        kids.extend(obj_group("funcs", "FUNCTIONS", "func", &objs.functions, &mut meta));
                    }
                }
            }
        }
        // While filtering, drop loaded schemas without a match (unloaded ones
        // stay visible with their loading placeholder until relations arrive).
        if filtering && node.relations.is_some() && matches == 0 {
            continue;
        }
        meta.insert(sid.clone(), NodeMeta::Schema { name: node.name.clone() });
        let exp = filtering || expanded.contains(&sid);
        schema_items.push(TreeItem::new(sid, node.name.clone()).children(kids).expanded(exp));
    }

    let db_id = SharedString::from("db");
    meta.insert(db_id.clone(), NodeMeta::Db);
    let exp = filtering || expanded.contains(&db_id);
    let db = TreeItem::new(db_id, dbname.to_string())
        .children(schema_items)
        .expanded(exp);
    (vec![db], meta)
}

/// Flat visible index of `id`, mirroring the widget's own flattening
/// (`TreeState::add_entry` DFS: an item counts, its children only if it is
/// expanded). Needed because `TreeState.entries` is private.
pub(super) fn flat_index_of(items: &[TreeItem], id: &str) -> Option<usize> {
    fn walk(item: &TreeItem, id: &str, ix: &mut usize) -> Option<usize> {
        if item.id.as_ref() == id {
            return Some(*ix);
        }
        *ix += 1;
        if item.is_expanded() {
            for child in &item.children {
                if let Some(found) = walk(child, id, ix) {
                    return Some(found);
                }
            }
        }
        None
    }
    let mut ix = 0;
    for item in items {
        if let Some(found) = walk(item, id, &mut ix) {
            return Some(found);
        }
    }
    None
}

/// Mirror widget-driven expansion changes (visible in the retained items via
/// the shared `Rc` state) into `expanded`. While filtering, db/schema
/// expansion is transient (forced by the filter) and skipped. Returns whether
/// anything changed.
pub(super) fn sync_expansion(
    items: &[TreeItem],
    filtering: bool,
    expanded: &mut HashSet<SharedString>,
) -> bool {
    fn walk(
        item: &TreeItem,
        filtering: bool,
        expanded: &mut HashSet<SharedString>,
        changed: &mut bool,
    ) {
        if !item.is_folder() {
            return;
        }
        let transient =
            filtering && (item.id.as_ref() == "db" || item.id.as_ref().starts_with(SCH_PREFIX));
        if !transient {
            let is = item.is_expanded();
            if is != expanded.contains(&item.id) {
                if is {
                    expanded.insert(item.id.clone());
                } else {
                    expanded.remove(&item.id);
                }
                *changed = true;
            }
        }
        for child in &item.children {
            walk(child, filtering, expanded, changed);
        }
    }
    let mut changed = false;
    for item in items {
        walk(item, filtering, expanded, &mut changed);
    }
    changed
}

impl Workspace {
    pub(super) fn load_schemas(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        self.spawn_db(cx, async move { db.schemas(conn).await }, |this, result, cx| {
            match result {
                Ok(schemas) => {
                    this.tree.schemas = schemas
                        .into_iter()
                        .map(|s| SchemaNode {
                            name: s.name,
                            relations: None,
                            objects: None,
                        })
                        .collect();
                    log(format!("schemas loaded: {}", this.tree.schemas.len()));
                    this.status = format!("{} schema(s)", this.tree.schemas.len());
                    // Expansion is kept across a refresh (relations of
                    // still-expanded schemas re-fetch via `sync_tree`).
                    this.tree.expanded.insert("db".into());
                    this.sync_tree(cx);
                    if std::env::var_os("ZDB_SELFTEST").is_some() {
                        this.selftest(cx);
                    }
                }
                Err(e) => this.status = format!("Failed to load schemas: {e}"),
            }
        });
    }

    /// Rebuild the widget's items from the model + `expanded_ids`, kick lazy
    /// loads for anything expanded-but-unloaded, and restore selection (or a
    /// pending reveal) across the `set_items` selection wipe. The single sink
    /// for every tree change: data arrival, filter edits, refresh, reveal.
    pub(super) fn sync_tree(&mut self, cx: &mut Context<Self>) {
        self.ensure_loads(cx);
        let (items, meta) = self.build_tree_items();
        self.tree.meta = meta;
        self.tree.items = items;
        let mut reveal = false;
        let sel_id = match &self.tree.pending_reveal {
            // Consume the reveal only once its node is actually visible
            // (schema loaded + expanded); until then keep the last selection.
            Some(rid) if flat_index_of(&self.tree.items, rid).is_some() => {
                reveal = true;
                self.tree.pending_reveal.take()
            }
            _ => self.tree.sel.clone(),
        };
        let sel_ix = sel_id.and_then(|id| flat_index_of(&self.tree.items, &id));
        let items = self.tree.items.clone();
        self.tree.state.update(cx, |ts, cx| {
            ts.set_items(items, cx);
            ts.set_selected_index(sel_ix, cx);
            if reveal {
                if let Some(ix) = sel_ix {
                    ts.scroll_to_item(ix, ScrollStrategy::Center);
                }
            }
        });
        cx.notify();
    }

    pub(super) fn build_tree_items(&self) -> (Vec<TreeItem>, NodeMetaMap) {
        let dbname = self
            .cfg
            .as_ref()
            .filter(|_| self.conn.is_some())
            .map(|c| c.dbname.as_str());
        build_tree(dbname, &self.tree.schemas, &self.tree.expanded, &self.tree.filter)
    }

    /// Start a lazy load for every expanded-but-unloaded schema/table not
    /// already in flight. While filtering, all schemas count as expanded so
    /// the search is global.
    pub(super) fn ensure_loads(&mut self, cx: &mut Context<Self>) {
        let filtering = !self.tree.filter.trim().is_empty();
        let mut schemas = Vec::new();
        let mut rels = Vec::new();
        for node in &self.tree.schemas {
            let sid = node_id(&["sch", &node.name]);
            let Some(relations) = &node.relations else {
                if (filtering || self.tree.expanded.contains(&sid))
                    && !self.tree.loads.contains(&sid)
                {
                    schemas.push(node.name.clone());
                }
                continue;
            };
            for rel in relations {
                if rel.kind != RelationKind::Table || rel.detail.is_some() {
                    continue;
                }
                let rid = node_id(&["rel", &node.name, &rel.name]);
                if self.tree.expanded.contains(&rid) && !self.tree.loads.contains(&rid) {
                    rels.push((node.name.clone(), rel.name.clone()));
                }
            }
        }
        for schema in schemas {
            self.load_relations(schema, cx);
        }
        for (schema, table) in rels {
            self.load_relation_detail(schema, table, cx);
        }
    }

    /// Tree-state observer: record the selection id (survives `set_items`)
    /// and mirror widget-driven toggles into `expanded_ids`, kicking loads
    /// for newly expanded unloaded nodes. Never calls `set_items` — the
    /// widget already shows the toggle via the shared item state, so there is
    /// no rebuild here (and thus no notify loop).
    pub(super) fn on_tree_notify(&mut self, state: Entity<TreeState>, cx: &mut Context<Self>) {
        self.tree.sel = state.read(cx).selected_entry().map(|e| e.item().id.clone());
        let filtering = !self.tree.filter.trim().is_empty();
        if sync_expansion(&self.tree.items, filtering, &mut self.tree.expanded) {
            self.ensure_loads(cx);
        }
    }

    /// Select + scroll the sidebar to `schema.table`, expanding ancestors and
    /// loading the schema's relations first if needed.
    pub(super) fn reveal_relation(&mut self, schema: &str, table: &str, cx: &mut Context<Self>) {
        if self.conn.is_none() {
            return;
        }
        self.tree.expanded.insert("db".into());
        self.tree.expanded.insert(node_id(&["sch", schema]));
        self.tree.expanded.insert(node_id(&["tbls", schema]));
        self.tree.pending_reveal = Some(node_id(&["rel", schema, table]));
        self.sync_tree(cx);
    }

    /// Enter on the tree: open the selected relation in a tab.
    pub(super) fn open_selected_tree_node(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .tree.state
            .read(cx)
            .selected_entry()
            .map(|e| e.item().id.clone())
        else {
            return;
        };
        let Some(NodeMeta::Rel { schema, name, .. }) = self.tree.meta.get(&id).cloned() else {
            return;
        };
        self.open_table_tab(schema, name, window, cx);
    }

    pub(super) fn load_relations(&mut self, schema: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let sid = node_id(&["sch", &schema]);
        if !self.tree.loads.insert(sid) {
            return; // already in flight
        }
        let db = self.db.clone();
        let sch = schema.clone();
        let fut = async move {
            let rels = db.relations(conn, sch.clone()).await;
            let objects = db.schema_objects(conn, sch).await;
            (rels, objects)
        };
        self.spawn_db(cx, fut, move |this, (rels, objects), cx| {
            this.tree.loads.remove(&node_id(&["sch", &schema]));
            match rels {
                Ok(rels) => {
                    // Look the node up by name: the tree may have been
                    // reloaded/reordered while the query ran.
                    if let Some(node) = this.tree.schemas.iter_mut().find(|n| n.name == schema) {
                        node.relations = Some(
                            rels.into_iter()
                                .map(|r| RelNode { name: r.name, kind: r.kind, detail: None })
                                .collect(),
                        );
                        node.objects = objects.ok();
                    }
                    // TABLES starts open (the primary content);
                    // SEQUENCES/FUNCTIONS start folded.
                    this.tree.expanded.insert(node_id(&["tbls", &schema]));
                }
                Err(e) => this.status = format!("Failed to load relations: {e}"),
            }
            this.sync_tree(cx);
        });
    }

    pub(super) fn load_relation_detail(&mut self, schema: String, table: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let rid = node_id(&["rel", &schema, &table]);
        if !self.tree.loads.insert(rid.clone()) {
            return; // already in flight
        }
        let db = self.db.clone();
        let (sch, tbl) = (schema.clone(), table.clone());
        let fut = async move { db.relation_detail(conn, sch, tbl).await };
        self.spawn_db(cx, fut, move |this, result, cx| {
            this.tree.loads.remove(&rid);
            match result {
                Ok(detail) => {
                    // Groups start folded; the user expands the one they need.
                    if let Some(node) = this
                        .tree
                        .schemas
                        .iter_mut()
                        .find(|n| n.name == schema)
                        .and_then(|s| s.relations.as_mut())
                        .and_then(|r| r.iter_mut().find(|r| r.name == table))
                    {
                        node.detail = Some(detail);
                    }
                }
                Err(e) => this.status = format!("Failed to load relation detail: {e}"),
            }
            this.sync_tree(cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    // ---- schema tree ---------------------------------------------------

    fn leaf(id: &str) -> TreeItem {
        TreeItem::new(id.to_string(), id.to_string())
    }

    fn sample_tree() -> Vec<SchemaNode> {
        vec![
            SchemaNode {
                name: "public".into(),
                relations: Some(vec![
                    RelNode { name: "users".into(), kind: RelationKind::Table, detail: None },
                    RelNode { name: "v_orders".into(), kind: RelationKind::View, detail: None },
                ]),
                objects: None,
            },
            SchemaNode { name: "audit".into(), relations: None, objects: None },
        ]
    }

    #[test]
    fn flat_index_mirrors_widget_flattening() {
        // Same semantics as the widget's own `test_tree_entry`: children are
        // visible (counted) only under an expanded parent.
        let items = vec![
            TreeItem::new("src", "src")
                .expanded(true)
                .child(TreeItem::new("src/ui", "ui").child(leaf("src/ui/button.rs")))
                .child(leaf("src/lib.rs")),
            leaf("Cargo.toml"),
        ];
        assert_eq!(flat_index_of(&items, "src"), Some(0));
        assert_eq!(flat_index_of(&items, "src/ui"), Some(1));
        assert_eq!(flat_index_of(&items, "src/ui/button.rs"), None); // collapsed
        assert_eq!(flat_index_of(&items, "src/lib.rs"), Some(2));
        assert_eq!(flat_index_of(&items, "Cargo.toml"), Some(3));
        assert_eq!(flat_index_of(&items, "nope"), None);
    }

    #[test]
    fn build_tree_shapes_and_filters() {
        let mut expanded = HashSet::new();
        expanded.insert(SharedString::from("db"));
        expanded.insert(node_id(&["sch", "public"]));

        let (items, meta) = build_tree(Some("zdb"), &sample_tree(), &expanded, "");
        assert_eq!(items.len(), 1);
        let db = &items[0];
        assert!(db.is_expanded());
        assert_eq!(db.children.len(), 2);
        let public = &db.children[0];
        assert!(public.is_expanded());
        // Relations sit under a TABLES group (folded here: not in `expanded`).
        assert_eq!(public.children.len(), 1);
        let tables = &public.children[0];
        assert_eq!(tables.label.as_ref(), "TABLES");
        assert!(!tables.is_expanded());
        assert_eq!(tables.children.len(), 2);
        // Unloaded table gets a loading placeholder so it stays expandable;
        // views are plain leaves.
        assert!(tables.children[0].is_folder());
        assert!(!tables.children[1].is_folder());
        // Unloaded schema: collapsed folder with a loading placeholder.
        let audit = &db.children[1];
        assert!(audit.is_folder());
        assert!(!audit.is_expanded());
        assert!(matches!(
            meta.get(&node_id(&["rel", "public", "users"])),
            Some(NodeMeta::Rel { .. })
        ));

        // Filtering: only matching relations remain; loaded schemas without a
        // match drop out, unloaded ones stay (results refine as loads land);
        // db + schemas + TABLES groups are force-expanded.
        let (items, _) = build_tree(Some("zdb"), &sample_tree(), &HashSet::new(), "user");
        let db = &items[0];
        assert!(db.is_expanded());
        let names: Vec<_> = db.children.iter().map(|c| c.label.to_string()).collect();
        assert_eq!(names, vec!["public", "audit"]);
        let tables = &db.children[0].children[0];
        assert!(tables.is_expanded());
        assert_eq!(tables.children.len(), 1);
        assert_eq!(tables.children[0].label.as_ref(), "users");

        // Not connected → no items at all.
        assert!(build_tree(None, &sample_tree(), &HashSet::new(), "").0.is_empty());
    }

    #[test]
    fn sync_expansion_mirrors_widget_toggles() {
        let mut expanded: HashSet<SharedString> = HashSet::new();
        let items = vec![TreeItem::new("db", "zdb")
            .child(TreeItem::new("sch\u{1}public", "public").child(leaf("x")))];
        // A widget toggle mutates the Rc<RefCell> state shared with our clone.
        items[0].clone().expanded(true);
        items[0].children[0].clone().expanded(true);
        assert!(sync_expansion(&items, false, &mut expanded));
        assert!(expanded.contains(&SharedString::from("db")));
        assert!(expanded.contains(&SharedString::from("sch\u{1}public")));
        // While filtering, db/schema expansion is transient: a widget-side
        // collapse must not touch expanded_ids.
        items[0].clone().expanded(false);
        items[0].children[0].clone().expanded(false);
        assert!(!sync_expansion(&items, true, &mut expanded));
        assert!(expanded.contains(&SharedString::from("db")));
        // The same collapse with no filter does sync.
        assert!(sync_expansion(&items, false, &mut expanded));
        assert!(!expanded.contains(&SharedString::from("db")));
    }

    #[gpui::test]
    fn opening_table_reveals_tree_node(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                // Fake a connected state with loaded relations (no real DB).
                ws.conn = Some(1);
                ws.cfg = Some(ConnectionConfig::new("dev", "h", "zdb", "u"));
                ws.tree.schemas = sample_tree();
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                assert!(ws.tree.expanded.contains(&SharedString::from("db")));
                assert!(ws.tree.expanded.contains(&node_id(&["sch", "public"])));
                assert!(ws.tree.pending_reveal.is_none(), "reveal consumed");
                let rel = node_id(&["rel", "public", "users"]);
                let ix = flat_index_of(&ws.tree.items, &rel).expect("node visible");
                assert_eq!(ws.tree.state.read(cx).selected_index(), Some(ix));
            })
            .unwrap();
    }

    #[gpui::test]
    fn widget_toggle_kicks_lazy_load(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.conn = Some(1);
                ws.cfg = Some(ConnectionConfig::new("dev", "h", "zdb", "u"));
                ws.tree.schemas = vec![SchemaNode { name: "public".into(), relations: None, objects: None }];
                ws.tree.expanded.insert("db".into());
                ws.sync_tree(cx);
                assert!(ws.tree.loads.is_empty(), "collapsed schema: nothing to load");
                // Simulate the widget expanding the schema row through the
                // shared item state + its notify.
                let sch = &ws.tree.items[0].children[0];
                assert!(sch.is_folder(), "loading placeholder keeps it expandable");
                sch.clone().expanded(true);
                ws.tree.state.update(cx, |_, cx| cx.notify());
            })
            .unwrap();
        // The observer runs when the first update's effects flush.
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.tree.expanded.contains(&node_id(&["sch", "public"])));
                assert!(ws.tree.loads.contains(&node_id(&["sch", "public"])));
            })
            .unwrap();
    }
}
