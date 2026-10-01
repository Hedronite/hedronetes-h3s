//! Server-Side Apply. Each manager declares the fields it owns; the object is
//! merged field by field, ownership is recorded in `metadata.managedFields`,
//! and a field two managers set to different values is a conflict unless the
//! request forces it. A field the applying manager no longer declares is
//! removed, which is the difference between apply and every merge patch.
use super::{http::Query, resources::Resource, Failure, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A position inside an object. A list whose items are objects carrying a
/// `name` is keyed by it, the way Kubernetes keys containers, volumes,
/// environment and mounts; every other list is owned as one field.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Segment {
    Field(String),
    Keyed(String, String),
}
type Path = Vec<Segment>;

const APPLY_JSON: &str = "application/apply-patch+json";
const APPLY_YAML: &str = "application/apply-patch+yaml";
const MANAGED: &str = "managedFields";
const FIELDS_TYPE: &str = "FieldsV1";

/// The apply request's identity and conflict policy.
pub(crate) struct Options {
    pub manager: String,
    pub force: bool,
}

/// The apply options for a request, or `None` when this is not an apply.
pub(crate) fn options(content_type: &str, query: &Query) -> Result<Option<Options>> {
    let content_type = content_type.split(';').next().unwrap_or("").trim();
    if content_type == APPLY_YAML {
        // A YAML body needs a YAML decoder; this crate has none in its
        // non-test dependencies, so the refusal is explicit rather than a
        // silent misread of the body.
        return Err(Failure::new(
            415,
            "UnsupportedMediaType",
            "send application/apply-patch+json: this server decodes JSON apply bodies only",
        ));
    }
    if content_type != APPLY_JSON {
        return Ok(None);
    }
    let manager = query
        .get("fieldManager")
        .map(str::to_owned)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            Failure::new(
                400,
                "BadRequest",
                "server-side apply requires the fieldManager query parameter",
            )
        })?;
    if manager.len() > 128 || manager.chars().any(char::is_control) {
        return Err(Failure::new(400, "BadRequest", "invalid fieldManager"));
    }
    let force = match query.get("force") {
        None => false,
        Some("true") => true,
        Some("false") => false,
        Some(_) => {
            return Err(Failure::new(
                400,
                "BadRequest",
                "force must be true or false",
            ))
        }
    };
    if let Some(validation) = query.get("fieldValidation") {
        if !matches!(validation, "Ignore" | "Warn" | "Strict") {
            return Err(Failure::new(
                400,
                "BadRequest",
                "fieldValidation must be Ignore, Warn or Strict",
            ));
        }
    }
    Ok(Some(Options { manager, force }))
}

/// The fields an applied object declares, at the granularity this server owns
/// them: leaves are owned individually, maps field by field, named lists item
/// by item, and everything else as a whole.
fn declared(value: &Value) -> BTreeSet<Path> {
    let mut out = BTreeSet::new();
    walk(value, false, &mut Vec::new(), &mut out);
    out
}
fn walk(value: &Value, under_metadata: bool, prefix: &mut Path, out: &mut BTreeSet<Path>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                // `managedFields` is the API's own bookkeeping, never applied.
                if under_metadata && key == MANAGED {
                    continue;
                }
                prefix.push(Segment::Field(key.clone()));
                walk(child, key == "metadata", prefix, out);
                prefix.pop();
            }
        }
        Value::Array(items) if !items.is_empty() && named(items) => {
            for item in items {
                let name = item["name"].as_str().unwrap_or_default().to_owned();
                prefix.push(Segment::Keyed("name".into(), name));
                walk(item, false, prefix, out);
                prefix.pop();
            }
        }
        // An empty object declares nothing. The typed decode of a request
        // materialises an absent required struct as `{}`, and treating that as
        // ownership of the whole subtree would clobber another manager's fields.
        Value::Object(_) => {}
        // Scalars, empty and unnamed lists: the field is owned as a whole.
        _ => {
            out.insert(prefix.clone());
        }
    }
}
fn named(items: &[Value]) -> bool {
    items
        .iter()
        .all(|item| item["name"].as_str().is_some_and(|n| !n.is_empty()))
}
fn get<'a>(root: &'a Value, path: &[Segment]) -> Option<&'a Value> {
    let mut current = root;
    for segment in path {
        current = match segment {
            Segment::Field(name) => current.get(name)?,
            Segment::Keyed(key, value) => item(current, key, value)?,
        };
    }
    Some(current)
}
fn item<'a>(current: &'a Value, key: &str, value: &str) -> Option<&'a Value> {
    current
        .as_array()?
        .iter()
        .find(|item| item[key].as_str() == Some(value))
}
fn set(root: &mut Value, path: &[Segment], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for (index, segment) in parents.iter().enumerate() {
        // A missing intermediate is created as the container its next segment
        // needs: a list for a keyed item, an object for a field.
        let keyed_next = matches!(path.get(index + 1), Some(Segment::Keyed(..)));
        current = match segment {
            Segment::Field(name) => {
                let map = current
                    .as_object_mut()
                    .expect("declared parent is an object");
                map.entry(name.clone()).or_insert_with(|| {
                    if keyed_next {
                        json!([])
                    } else {
                        json!({})
                    }
                })
            }
            Segment::Keyed(key, key_value) => {
                let array = current.as_array_mut().expect("declared parent is a list");
                let index = array
                    .iter()
                    .position(|item| item[key].as_str() == Some(key_value.as_str()))
                    .unwrap_or_else(|| {
                        array.push(json!({key.clone(): key_value.clone()}));
                        array.len() - 1
                    });
                &mut array[index]
            }
        };
    }
    match last {
        Segment::Field(name) => {
            current
                .as_object_mut()
                .expect("declared parent is an object")
                .insert(name.clone(), value);
        }
        Segment::Keyed(key, key_value) => {
            let array = current.as_array_mut().expect("declared parent is a list");
            let index = array
                .iter()
                .position(|item| item[key].as_str() == Some(key_value.as_str()))
                .unwrap_or_else(|| {
                    array.push(json!({key.clone(): key_value.clone()}));
                    array.len() - 1
                });
            array[index] = value;
        }
    }
}
fn remove(root: &mut Value, path: &[Segment]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        let next = match segment {
            Segment::Field(name) => current.get_mut(name),
            Segment::Keyed(key, key_value) => current
                .as_array_mut()
                .and_then(|array| {
                    array
                        .iter_mut()
                        .find(|item| item[key].as_str() == Some(key_value.as_str()))
                })
                .map(|item| &mut *item),
        };
        match next {
            Some(next) => current = next,
            None => return,
        }
    }
    match last {
        Segment::Field(name) => {
            if let Some(map) = current.as_object_mut() {
                map.remove(name);
            }
        }
        Segment::Keyed(key, key_value) => {
            if let Some(array) = current.as_array_mut() {
                array.retain(|item| item[key].as_str() != Some(key_value.as_str()));
            }
        }
    }
}
/// Drop list items that only carry their own key once their fields are gone.
fn prune(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for child in map.values_mut() {
                prune(child);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                prune(item);
            }
            items.retain(|item| match item.as_object() {
                Some(map) => !map.is_empty() && !(map.len() == 1 && map.contains_key("name")),
                None => true,
            });
        }
        _ => {}
    }
}

/// One `managedFields` entry, reduced to the paths it owns.
struct Entry {
    manager: String,
    operation: String,
    api_version: String,
    paths: BTreeSet<Path>,
}
fn entries(stored: &Value) -> Vec<Entry> {
    stored["metadata"][MANAGED]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let manager = entry["manager"].as_str()?.to_owned();
            let operation = entry["operation"].as_str().unwrap_or("Update").to_owned();
            let api_version = entry["apiVersion"].as_str().unwrap_or("").to_owned();
            let mut paths = BTreeSet::new();
            if entry["fieldsType"].as_str() == Some(FIELDS_TYPE) {
                tree(&entry["fieldsV1"], &mut Vec::new(), &mut paths);
            }
            Some(Entry {
                manager,
                operation,
                api_version,
                paths,
            })
        })
        .collect()
}
fn tree(value: &Value, prefix: &mut Path, out: &mut BTreeSet<Path>) {
    match value {
        Value::Object(map) if !map.is_empty() => {
            for (key, child) in map {
                if let Some(name) = key.strip_prefix("f:") {
                    prefix.push(Segment::Field(name.to_owned()));
                } else if let Some(keyed) = key.strip_prefix("k:") {
                    let Ok(parsed) = serde_json::from_str::<Value>(keyed) else {
                        continue;
                    };
                    let Some((key, value)) = parsed.as_object().and_then(|m| {
                        m.iter()
                            .next()
                            .and_then(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                    }) else {
                        continue;
                    };
                    prefix.push(Segment::Keyed(key, value));
                } else {
                    continue;
                }
                tree(child, prefix, out);
                prefix.pop();
            }
        }
        _ => {
            out.insert(prefix.clone());
        }
    }
}
fn fields_v1(paths: &BTreeSet<Path>) -> Value {
    let mut root = json!({});
    for path in paths {
        let mut current = &mut root;
        for segment in path {
            let key = match segment {
                Segment::Field(name) => format!("f:{name}"),
                Segment::Keyed(key, value) => format!("k:{}", json!({key.clone(): value.clone()})),
            };
            current = current
                .as_object_mut()
                .expect("fields tree")
                .entry(key)
                .or_insert_with(|| json!({}));
        }
    }
    root
}
fn describe(path: &Path) -> String {
    path.iter()
        .map(|segment| match segment {
            Segment::Field(name) => name.clone(),
            Segment::Keyed(_, value) => format!("[{value}]"),
        })
        .collect::<Vec<_>>()
        .join(".")
}
fn owner_of(entries: &[Entry], path: &Path, manager: &str) -> Option<String> {
    for entry in entries {
        if entry.manager == manager || entry.operation != "Apply" {
            continue;
        }
        for owned in &entry.paths {
            if path.starts_with(owned) {
                return Some(entry.manager.clone());
            }
        }
    }
    None
}

/// Merge an applied object into what is stored, recording ownership.
pub(crate) fn server_side(
    _resource: Resource,
    stored: Value,
    mut applied: Value,
    options: &Options,
) -> Result<Value> {
    if let Some(metadata) = applied["metadata"].as_object_mut() {
        metadata.remove(MANAGED);
        metadata.remove("resourceVersion");
        metadata.remove("uid");
        metadata.remove("creationTimestamp");
    }
    let declared = declared(&applied);
    let mut existing = entries(&stored);
    let previous = existing
        .iter()
        .find(|entry| entry.manager == options.manager && entry.operation == "Apply")
        .map(|entry| entry.paths.clone())
        .unwrap_or_default();
    // A field another manager owns and this apply changes is a conflict, unless
    // the request forces it, in which case ownership moves here.
    for path in &declared {
        let Some(owner) = owner_of(&existing, path, &options.manager) else {
            continue;
        };
        let unchanged = get(&stored, path) == get(&applied, path);
        if unchanged {
            continue;
        }
        if !options.force {
            return Err(Failure::new(
                409,
                "Conflict",
                format!(
                    "field {} is managed by {owner}; reapply with force to take it over",
                    describe(path)
                ),
            ));
        }
        for entry in existing.iter_mut() {
            if entry.manager == owner {
                entry.paths.retain(|owned| path.starts_with(owned));
            }
        }
    }
    // The declaration replaces this manager's previous one: fields it no longer
    // declares are removed, and every declared field takes the applied value.
    let mut result = stored;
    for path in &previous {
        if !declared.contains(path) && !declared.iter().any(|d| d.starts_with(path)) {
            remove(&mut result, path);
        }
    }
    prune(&mut result);
    for path in &declared {
        if let Some(value) = get(&applied, path) {
            set(&mut result, path, value.clone());
        }
    }
    // Rebuild `managedFields`: this manager's entry is replaced, the entries
    // whose fields are all gone disappear, and every other entry is untouched.
    existing.retain(|entry| entry.manager != options.manager || entry.operation != "Apply");
    existing.push(Entry {
        manager: options.manager.clone(),
        operation: "Apply".into(),
        api_version: applied["apiVersion"].as_str().unwrap_or("").to_owned(),
        paths: declared,
    });
    existing.retain(|entry| !entry.paths.is_empty());
    existing.sort_by(|a, b| a.manager.cmp(&b.manager));
    let managed: Vec<Value> = existing
        .iter()
        .map(|entry| {
            json!({
                "manager": entry.manager,
                "operation": entry.operation,
                "apiVersion": entry.api_version,
                "time": crate::now(),
                "fieldsType": FIELDS_TYPE,
                "fieldsV1": fields_v1(&entry.paths),
            })
        })
        .collect();
    if !managed.is_empty() {
        if !result["metadata"].is_object() {
            result["metadata"] = json!({});
        }
        result["metadata"][MANAGED] = json!(managed);
    }
    let _ = BTreeMap::<String, String>::new();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(replicas: i64, image: &str) -> Value {
        json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","labels":{"app":"web"}},"spec":{"replicas":replicas,"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"web","image":image}]}}}})
    }
    fn apply(stored: &Value, applied: Value, manager: &str, force: bool) -> Result<Value> {
        server_side(
            *super::super::resources::RESOURCES
                .iter()
                .find(|r| r.kind == "Deployment")
                .unwrap(),
            stored.clone(),
            applied,
            &Options {
                manager: manager.into(),
                force,
            },
        )
    }

    #[test]
    fn a_manager_owns_what_it_declares_and_keeps_it_across_another_apply() {
        let empty = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"}});
        let first = apply(&empty, deployment(2, "web:v1"), "a", false).unwrap();
        let managed = first["metadata"]["managedFields"].as_array().unwrap();
        assert_eq!(managed.len(), 1);
        assert_eq!(managed[0]["manager"], "a");
        assert_eq!(managed[0]["operation"], "Apply");
        assert_eq!(
            managed[0]["fieldsV1"]["f:spec"]["f:replicas"],
            json!({}),
            "{managed:?}"
        );
        // A second manager applies a field of its own: the first manager's
        // entry and values survive, and both entries are recorded.
        let mut annotation = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","annotations":{"team":"b"}}});
        annotation["spec"] = json!({"template":{"metadata":{"annotations":{"team":"b"}}}});
        let second = apply(&first, annotation, "b", false).unwrap();
        let managers: Vec<&str> = second["metadata"]["managedFields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["manager"].as_str().unwrap())
            .collect();
        assert_eq!(managers, ["a", "b"]);
        assert_eq!(second["spec"]["replicas"], 2);
        assert_eq!(second["metadata"]["annotations"]["team"], "b");
        assert_eq!(
            second["spec"]["template"]["metadata"]["labels"]["app"],
            "web"
        );
        // The first manager reapplies its own image: only that changes, and the
        // second manager's annotation is untouched.
        let third = apply(&second, deployment(2, "web:v2"), "a", false).unwrap();
        assert_eq!(
            third["spec"]["template"]["spec"]["containers"][0]["image"],
            "web:v2"
        );
        assert_eq!(third["spec"]["replicas"], 2);
        assert_eq!(third["metadata"]["annotations"]["team"], "b");
    }
    /// A partial apply declares a field of its own and nothing else; every
    /// field another manager owns, and every field it never declared, survives.
    #[test]
    fn a_partial_apply_keeps_what_it_does_not_declare() {
        let empty = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"}});
        let created = apply(&empty, deployment(2, "web:v1"), "a", false).unwrap();
        let partial = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web","annotations":{"team":"b"}},
            "spec":{"template":{"metadata":{"annotations":{"team":"b"}}}}});
        let merged = apply(&created, partial, "b", false).unwrap();
        assert_eq!(
            merged["spec"]["selector"]["matchLabels"]["app"], "web",
            "{merged}"
        );
        assert_eq!(merged["spec"]["replicas"], 2, "{merged}");
        assert_eq!(
            merged["spec"]["template"]["spec"]["containers"][0]["image"], "web:v1",
            "{merged}"
        );
        assert_eq!(
            merged["spec"]["template"]["metadata"]["annotations"]["team"], "b",
            "{merged}"
        );
        assert_eq!(
            merged["spec"]["template"]["metadata"]["labels"]["app"], "web",
            "{merged}"
        );
    }
    #[test]
    fn a_contested_field_conflicts_until_the_apply_forces_it() {
        let empty = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"}});
        let first = apply(&empty, deployment(2, "web:v1"), "a", false).unwrap();
        let conflict = apply(&first, deployment(9, "web:v1"), "b", false).unwrap_err();
        assert_eq!(conflict.code, 409);
        assert!(
            conflict.message.contains("managed by a"),
            "{}",
            conflict.message
        );
        assert_eq!(first["spec"]["replicas"], 2);
        let forced = apply(&first, deployment(9, "web:v1"), "b", true).unwrap();
        assert_eq!(forced["spec"]["replicas"], 9);
        assert!(forced["metadata"]["managedFields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["manager"] == "b"));
        // A field this manager no longer declares is removed on reapply.
        let mut dropped = deployment(9, "web:v1");
        dropped["spec"]["replicas"] = Value::Null;
        dropped["spec"].as_object_mut().unwrap().remove("replicas");
        let pruned = apply(&forced, dropped, "b", false).unwrap();
        assert!(pruned["spec"].get("replicas").is_none(), "{pruned}");
    }
}
