use std::{
    collections::HashMap,
    fs::{self},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use ahash::AHashMap;
use itertools::Itertools;
use std::collections::HashSet;

use seekstorm::{
    commit::Commit,
    highlighter::{highlighter, Highlight},
    index::{
        create_index, open_index, AccessType, DeleteDocument, DeleteDocuments,
        DeleteDocumentsByQuery, DistanceField, Document, Facet, FileType, IndexArc, IndexDocument,
        IndexDocuments, IndexMetaObject, MinMaxFieldJson, SchemaField, SimilarityType, Synonym,
        TokenizerType, UpdateDocument, UpdateDocuments,
    },
    ingest::IndexPdfBytes,
    search::{FacetFilter, QueryFacet, QueryType, ResultSort, ResultType, Search},
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::{
    http_server::calculate_hash,
    multi_tenancy::{ApikeyObject, ApikeyQuotaObject},
    VERSION,
};

const APIKEY_PATH: &str = "apikey.json";

#[derive(Deserialize, Serialize, Clone)]
pub struct SearchRequestObject {
    #[serde(rename = "query")]
    pub query_string: String,
    pub offset: usize,
    pub length: usize,
    #[serde(default)]
    pub result_type: ResultType,
    #[serde(default)]
    pub realtime: bool,
    #[serde(default)]
    pub highlights: Vec<Highlight>,
    #[serde(default)]
    pub field_filter: Vec<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub distance_fields: Vec<DistanceField>,
    #[serde(default)]
    pub query_facets: Vec<QueryFacet>,
    #[serde(default)]
    pub facet_filter: Vec<FacetFilter>,
    #[serde(default)]
    pub result_sort: Vec<ResultSort>,
    #[serde(default = "query_type_api")]
    pub query_type_default: QueryType,
}

fn query_type_api() -> QueryType {
    QueryType::Intersection
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchResultObject {
    pub time: u128,
    pub query: String,
    pub offset: usize,
    pub length: usize,
    pub count: usize,
    pub count_total: usize,
    pub query_terms: Vec<String>,
    pub results: Vec<Document>,
    pub facets: AHashMap<String, Facet>,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateIndexRequest {
    pub index_name: String,
    #[serde(default)]
    pub schema: Vec<SchemaField>,
    #[serde(default = "similarity_type_api")]
    pub similarity: SimilarityType,
    #[serde(default = "tokenizer_type_api")]
    pub tokenizer: TokenizerType,
    #[serde(default)]
    pub synonyms: Vec<Synonym>,
}

fn similarity_type_api() -> SimilarityType {
    SimilarityType::Bm25fProximity
}

fn tokenizer_type_api() -> TokenizerType {
    TokenizerType::UnicodeAlphanumeric
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeleteApikeyRequest {
    pub apikey_base64: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GetDocumentRequest {
    #[serde(default)]
    pub query_terms: Vec<String>,
    #[serde(default)]
    pub highlights: Vec<Highlight>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub distance_fields: Vec<DistanceField>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct IndexResponseObject {
    pub id: u64,
    pub name: String,
    pub schema: HashMap<String, SchemaField>,
    pub indexed_doc_count: usize,
    pub operations_count: u64,
    pub query_count: u64,
    pub version: String,
    pub facets_minmax: HashMap<String, MinMaxFieldJson>,
}

/// Save file atomically
pub(crate) fn save_file_atomically(path: &PathBuf, content: String) {
    let mut temp_path = path.clone();
    temp_path.set_extension("bak");
    fs::write(&temp_path, content).unwrap();
    match fs::rename(temp_path, path) {
        Ok(_) => {}
        Err(e) => println!("error: {e:?}"),
    }
}

pub(crate) fn save_apikey_data(apikey: &ApikeyObject, index_path: &PathBuf) {
    let apikey_id: u64 = apikey.id;

    let apikey_id_path = Path::new(&index_path).join(apikey_id.to_string());
    let apikey_persistence_json = serde_json::to_string(&apikey).unwrap();
    let apikey_persistence_path = Path::new(&apikey_id_path).join(APIKEY_PATH);
    save_file_atomically(&apikey_persistence_path, apikey_persistence_json);
}

pub(crate) fn create_apikey_api<'a>(
    index_path: &'a PathBuf,
    apikey_quota_request_object: ApikeyQuotaObject,
    apikey: &[u8],
    apikey_list: &'a mut HashMap<u128, ApikeyObject>,
) -> &'a mut ApikeyObject {
    let apikey_hash_u128 = calculate_hash(&apikey) as u128;

    let mut apikey_id: u64 = 0;
    let mut apikey_list_vec: Vec<(&u128, &ApikeyObject)> = apikey_list.iter().collect();
    apikey_list_vec.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    for value in apikey_list_vec {
        if value.1.id == apikey_id {
            apikey_id = value.1.id + 1;
        } else {
            break;
        }
    }

    let apikey_object = ApikeyObject {
        id: apikey_id,
        apikey_hash: apikey_hash_u128,
        quota: apikey_quota_request_object,
        index_list: HashMap::new(),
    };

    let apikey_id_path = Path::new(&index_path).join(apikey_id.to_string());
    fs::create_dir_all(apikey_id_path).unwrap();

    save_apikey_data(&apikey_object, index_path);

    apikey_list.insert(apikey_hash_u128, apikey_object);
    apikey_list.get_mut(&apikey_hash_u128).unwrap()
}

pub(crate) fn delete_apikey_api(
    index_path: &PathBuf,
    apikey_list: &mut HashMap<u128, ApikeyObject>,
    apikey_hash: u128,
) -> Result<u64, String> {
    if let Some(apikey_object) = apikey_list.get(&apikey_hash) {
        let apikey_id_path = Path::new(&index_path).join(apikey_object.id.to_string());
        println!("delete path {}", apikey_id_path.to_string_lossy());
        fs::remove_dir_all(&apikey_id_path).unwrap();

        apikey_list.remove(&apikey_hash);
        Ok(apikey_list.len() as u64)
    } else {
        Err("not found".to_string())
    }
}

/// Open all indices below a single apikey
pub(crate) async fn open_all_indices(
    index_path: &PathBuf,
    index_list: &mut HashMap<u64, IndexArc>,
) {
    if !Path::exists(index_path) {
        fs::create_dir_all(index_path).unwrap();
    }

    for result in fs::read_dir(index_path).unwrap() {
        let path = result.unwrap();
        if path.path().is_dir() {
            let single_index_path = path.path();
            let Ok(index_arc) = open_index(&single_index_path, false).await else {
                continue;
            };

            let index_id = index_arc.read().await.meta.id;
            index_list.insert(index_id, index_arc);
        }
    }
}

/// Open api key
pub(crate) async fn open_apikey(
    index_path: &PathBuf,
    apikey_list: &mut HashMap<u128, ApikeyObject>,
) -> bool {
    let apikey_path = Path::new(&index_path).join(APIKEY_PATH);
    match fs::read_to_string(apikey_path) {
        Ok(apikey_string) => {
            let mut apikey_object: ApikeyObject = serde_json::from_str(&apikey_string).unwrap();

            open_all_indices(index_path, &mut apikey_object.index_list).await;
            apikey_list.insert(apikey_object.apikey_hash, apikey_object);

            true
        }
        Err(_) => false,
    }
}

/// Open all apikeys in the specified path
pub(crate) async fn open_all_apikeys(
    index_path: &PathBuf,
    apikey_list: &mut HashMap<u128, ApikeyObject>,
) -> bool {
    let mut test_index_flag = false;
    if !Path::exists(index_path) {
        println!("index path not found: {} ", index_path.to_string_lossy());
        fs::create_dir_all(index_path).unwrap();
    }

    for result in fs::read_dir(index_path).unwrap() {
        let path = result.unwrap();
        if path.path().is_dir() {
            let single_index_path = path.path();
            test_index_flag |= open_apikey(&single_index_path, apikey_list).await;
        }
    }
    test_index_flag
}

pub(crate) fn create_index_api<'a>(
    index_path: &'a PathBuf,
    index_name: String,
    schema: Vec<SchemaField>,
    similarity: SimilarityType,
    tokenizer: TokenizerType,
    synonyms: Vec<Synonym>,
    apikey_object: &'a mut ApikeyObject,
) -> u64 {
    let mut index_id: u64 = 0;
    for id in apikey_object.index_list.keys().sorted() {
        if *id == index_id {
            index_id = id + 1;
        } else {
            break;
        }
    }

    let index_id_path = Path::new(&index_path)
        .join(apikey_object.id.to_string())
        .join(index_id.to_string());
    fs::create_dir_all(&index_id_path).unwrap();

    let meta = IndexMetaObject {
        id: index_id,
        name: index_name,
        similarity,
        tokenizer,
        access_type: AccessType::Mmap,
    };

    let index = create_index(&index_id_path, meta, &schema, true, &synonyms, 11, false).unwrap();

    let index_arc = Arc::new(RwLock::new(index));
    apikey_object.index_list.insert(index_id, index_arc);

    index_id
}

pub(crate) async fn delete_index_api(
    index_id: u64,
    index_list: &mut HashMap<u64, IndexArc>,
) -> Result<u64, String> {
    if let Some(index_arc) = index_list.get(&index_id) {
        let mut index_mut = index_arc.write().await;
        index_mut.delete_index();
        drop(index_mut);
        index_list.remove(&index_id);

        Ok(index_list.len() as u64)
    } else {
        Err("index_id not found".to_string())
    }
}

pub(crate) async fn commit_index_api(index_arc: &IndexArc) -> Result<u64, String> {
    let mut index_arc_clone = index_arc.clone();
    let index_ref = index_arc.read().await;
    let indexed_doc_count = index_ref.indexed_doc_count;

    drop(index_ref);
    index_arc_clone.commit().await;

    Ok(indexed_doc_count as u64)
}

pub(crate) async fn close_index_api(index_arc: &IndexArc) -> Result<u64, String> {
    let mut index_mut = index_arc.write().await;
    let indexed_doc_count = index_mut.indexed_doc_count;
    index_mut.close_index();
    drop(index_mut);

    Ok(indexed_doc_count as u64)
}

pub(crate) async fn set_synonyms_api(
    index_arc: &IndexArc,
    synonyms: Vec<Synonym>,
) -> Result<usize, String> {
    let mut index_mut = index_arc.write().await;
    index_mut.set_synonyms(&synonyms)
}

pub(crate) async fn add_synonyms_api(
    index_arc: &IndexArc,
    synonyms: Vec<Synonym>,
) -> Result<usize, String> {
    let mut index_mut = index_arc.write().await;
    index_mut.add_synonyms(&synonyms)
}

pub(crate) async fn get_synonyms_api(index_arc: &IndexArc) -> Result<Vec<Synonym>, String> {
    let index_ref = index_arc.read().await;
    index_ref.get_synonyms()
}

pub(crate) async fn get_index_stats_api(
    _index_path: &Path,
    index_id: u64,
    index_list: &HashMap<u64, IndexArc>,
) -> Result<IndexResponseObject, String> {
    if let Some(index_arc) = index_list.get(&index_id) {
        let index_ref = index_arc.read().await;

        Ok(IndexResponseObject {
            version: VERSION.to_string(),
            schema: index_ref.schema_map.clone(),
            id: index_ref.meta.id,
            name: index_ref.meta.name.clone(),
            indexed_doc_count: index_ref.indexed_doc_count,
            operations_count: 0,
            query_count: 0,
            facets_minmax: index_ref.get_index_facets_minmax(),
        })
    } else {
        Err("index_id not found".to_string())
    }
}

pub(crate) async fn get_all_index_stats_api(
    _index_path: &Path,
    _index_list: &HashMap<u64, IndexArc>,
) -> Result<Vec<IndexResponseObject>, String> {
    Err("err".to_string())
}

pub(crate) async fn index_document_api(
    index_arc: &IndexArc,
    document: Document,
) -> Result<usize, String> {
    index_arc.index_document(document, FileType::None).await;
    Ok(index_arc.read().await.indexed_doc_count)
}

pub(crate) async fn index_file_api(
    index_arc: &IndexArc,
    file_path: &Path,
    file_date: i64,
    document: &[u8],
) -> Result<usize, String> {
    match index_arc
        .index_pdf_bytes(file_path, file_date, document)
        .await
    {
        Ok(_) => Ok(index_arc.read().await.indexed_doc_count),
        Err(e) => Err(e),
    }
}

pub(crate) async fn get_file_api(index_arc: &IndexArc, document_id: usize) -> Option<Vec<u8>> {
    if !index_arc.read().await.stored_field_names.is_empty() {
        match index_arc.read().await.get_file(document_id) {
            Ok(doc) => Some(doc),
            Err(_e) => None,
        }
    } else {
        None
    }
}

pub(crate) async fn index_documents_api(
    index_arc: &IndexArc,
    document_vec: Vec<Document>,
) -> Result<usize, String> {
    index_arc.index_documents(document_vec).await;
    Ok(index_arc.read().await.indexed_doc_count)
}

pub(crate) async fn get_document_api(
    index_arc: &IndexArc,
    document_id: usize,
    get_document_request: GetDocumentRequest,
) -> Option<Document> {
    if !index_arc.read().await.stored_field_names.is_empty() {
        let highlighter_option = if get_document_request.highlights.is_empty()
            || get_document_request.query_terms.is_empty()
        {
            None
        } else {
            Some(
                highlighter(
                    index_arc,
                    get_document_request.highlights,
                    get_document_request.query_terms,
                )
                .await,
            )
        };

        match index_arc.read().await.get_document(
            document_id,
            true,
            &highlighter_option,
            &HashSet::from_iter(get_document_request.fields),
            &get_document_request.distance_fields,
        ) {
            Ok(doc) => Some(doc),
            Err(_e) => None,
        }
    } else {
        None
    }
}

pub(crate) async fn update_document_api(
    index_arc: &IndexArc,
    id_document: (u64, Document),
) -> Result<u64, String> {
    index_arc.update_document(id_document).await;
    Ok(index_arc.read().await.indexed_doc_count as u64)
}

pub(crate) async fn update_documents_api(
    index_arc: &IndexArc,
    id_document_vec: Vec<(u64, Document)>,
) -> Result<u64, String> {
    index_arc.update_documents(id_document_vec).await;
    Ok(index_arc.read().await.indexed_doc_count as u64)
}

pub(crate) async fn delete_document_api(
    index_arc: &IndexArc,
    document_id: u64,
) -> Result<u64, String> {
    index_arc.delete_document(document_id).await;
    Ok(index_arc.read().await.indexed_doc_count as u64)
}

pub(crate) async fn delete_documents_api(
    index_arc: &IndexArc,
    document_id_vec: Vec<u64>,
) -> Result<u64, String> {
    index_arc.delete_documents(document_id_vec).await;
    Ok(index_arc.read().await.indexed_doc_count as u64)
}

pub(crate) async fn delete_documents_by_query_api(
    index_arc: &IndexArc,
    search_request: SearchRequestObject,
) -> Result<u64, String> {
    index_arc
        .delete_documents_by_query(
            search_request.query_string.to_owned(),
            search_request.query_type_default,
            search_request.offset,
            search_request.length,
            search_request.realtime,
            search_request.field_filter,
            search_request.facet_filter,
            search_request.result_sort,
        )
        .await;

    Ok(index_arc.read().await.indexed_doc_count as u64)
}

pub(crate) async fn query_index_api(
    index_arc: &IndexArc,
    search_request: SearchRequestObject,
) -> SearchResultObject {
    let start_time = Instant::now();

    let result_object = index_arc
        .search(
            search_request.query_string.to_owned(),
            search_request.query_type_default,
            search_request.offset,
            search_request.length,
            search_request.result_type,
            search_request.realtime,
            search_request.field_filter,
            search_request.query_facets,
            search_request.facet_filter,
            search_request.result_sort,
        )
        .await;

    let elapsed_time = start_time.elapsed().as_nanos();

    let return_fields_filter = HashSet::from_iter(search_request.fields);

    let mut results: Vec<Document> = Vec::new();

    if !index_arc.read().await.stored_field_names.is_empty() {
        let highlighter_option = if search_request.highlights.is_empty() {
            None
        } else {
            Some(
                highlighter(
                    index_arc,
                    search_request.highlights,
                    result_object.query_terms.clone(),
                )
                .await,
            )
        };

        for result in result_object.results.iter() {
            match index_arc.read().await.get_document(
                result.doc_id,
                search_request.realtime,
                &highlighter_option,
                &return_fields_filter,
                &search_request.distance_fields,
            ) {
                Ok(doc) => {
                    let mut doc = doc;
                    doc.insert("_id".to_string(), result.doc_id.into());
                    doc.insert("_score".to_string(), result.score.into());

                    results.push(doc);
                }
                Err(_e) => {}
            }
        }
    }

    SearchResultObject {
        query: search_request.query_string.to_owned(),
        time: elapsed_time,
        offset: search_request.offset,
        length: search_request.length,
        count: result_object.results.len(),
        count_total: result_object.result_count_total as usize,
        query_terms: result_object.query_terms,
        results,
        facets: result_object.facets,
        suggestions: Vec::new(),
    }
}
