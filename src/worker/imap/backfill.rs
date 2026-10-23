//! The background index (#429): the rest of each folder past the first
//! page, a chunk at a time, and the one-time References repair.

use async_imap::types::Fetch;
use futures::TryStreamExt;

use crate::cache::Cache;
use crate::config::AccountConfig;
use crate::models::Message;
use crate::worker::ids::merge_msgids;
use crate::worker::imap::connect::connect;
use crate::worker::imap::fetch_mode::{is_parse_error, step_fetch_mode};
use crate::worker::imap::gmail::read_gmail_labels;
use crate::worker::imap::prefetch::queue_attachment_prefetch;
use crate::worker::imap::summary::{build_summary, references_of, summary_from_headers};
use crate::worker::imap::wire::{fetch_uids, search_uids, sel, uid_set};
use crate::worker::imap::{ImapSession, BACKFILL_CHUNK, REFS_FETCH_ITEM};
use crate::worker::WorkerEvent;

/// How many messages one References-repair step asks the server about. Each row
/// costs only its References header, so this can be larger than an envelope
/// chunk without making the connection unresponsive to real requests.
const REFS_REPAIR_CHUNK: usize = 500;

/// One step of the one-time References repair for the folder at the head of the
/// queue (#21, #42).
///
/// Messages indexed before Hylki asked for References hold only In-Reply-To,
/// which names the immediate parent — and for an incoming reply that parent is
/// usually your own message in Sent, so the link points outside the folder and
/// the reply starts a thread of its own. This asks the server for the one header
/// that fixes it, for the replies that can still be improved, newest first.
///
/// The watermark makes it resumable and run exactly once per folder; a repaired
/// row re-threads the next time its folder is read from cache.
pub(super) async fn run_one_refs_repair(
    queue: &mut std::collections::VecDeque<(u32, String)>,
    session: &mut Option<ImapSession>,
    account_id: u32,
    cache: Option<&Cache>,
    emit: &impl Fn(WorkerEvent),
) {
    let Some((folder_id, path)) = queue.pop_front() else {
        return;
    };
    let Some(c) = cache else { return }; // nothing to repair without a cache
    let Some(mut s) = session.take() else {
        queue.push_front((folder_id, path));
        return;
    };

    let (below, _) = c.refs_repair_state(account_id, &path);
    let chunk = c.uids_needing_references(account_id, &path, below, REFS_REPAIR_CHUNK);
    if chunk.is_empty() {
        c.set_refs_repair_state(account_id, &path, 0, true);
        *session = Some(s); // folder done — don't requeue
        return;
    }

    let set = uid_set(&chunk.iter().map(|(uid, _)| *uid).collect::<Vec<_>>());
    let fetched: Result<Vec<Fetch>, _> = async {
        sel(&mut s, &path).await?;
        fetch_uids(&mut s, &set, format!("(UID{REFS_FETCH_ITEM})"))
            .await?
            .try_collect()
            .await
    }
    .await;
    let Ok(fetches) = fetched else {
        // Leave the watermark alone and try this folder again later.
        queue.push_back((folder_id, path));
        *session = Some(s);
        return;
    };

    let found: std::collections::HashMap<u32, String> = fetches
        .iter()
        .filter_map(|f| f.uid.map(|uid| (uid, references_of(f))))
        .collect();
    let mut repaired = 0usize;
    for (uid, existing) in &chunk {
        if let Some(refs) = found.get(uid) {
            let merged = merge_msgids(refs, existing);
            if &merged != existing {
                c.set_references(account_id, &path, *uid, &merged);
                repaired += 1;
            }
        }
    }
    // Walk on from the oldest uid this chunk covered.
    let lowest = chunk.iter().map(|(uid, _)| *uid).min().unwrap_or(0);
    c.set_refs_repair_state(account_id, &path, lowest, false);
    queue.push_back((folder_id, path));
    *session = Some(s);
    if repaired > 0 {
        // The folder's cached rows have changed underneath whatever is on
        // screen; re-emitting lets the list re-thread with what was found.
        emit(WorkerEvent::RefsRepaired { folder_id });
    }
}

/// A background job to index the rest of a folder (everything past the fast first
/// page) so search covers the whole mailbox. `remaining` is the still-to-fetch
/// UIDs (newest first), computed lazily on the first drain.
#[derive(Clone)]
pub(in crate::worker) struct Backfill {
    pub(in crate::worker) folder_id: u32,
    pub(in crate::worker) path: String,
    pub(in crate::worker) remaining: Option<Vec<u32>>,
    /// Whether this folder feeds the attachments gallery (not Trash/Junk/Drafts);
    /// if so, its backfilled messages' attachments are prefetched too.
    pub(in crate::worker) gallery: bool,
}

// select! may receive a reader before polling the job at all. In that case
// the original is still queued; otherwise restore the snapshot, including any
// chunk the cancelled future drained before the network response arrived.
pub(in crate::worker) fn restore_interrupted_backfill(
    queue: &mut std::collections::VecDeque<Backfill>,
    job: Backfill,
) {
    if !queue.iter().any(|queued| queued.path == job.path) {
        queue.push_front(job);
    }
}

/// Determine which UIDs still need indexing: everything on the server not already
/// cached. Also reconciles deletions (cached UIDs the server no longer has).
async fn backfill_worklist(
    session: &mut ImapSession,
    account_id: u32,
    path: &str,
    cache: Option<&Cache>,
) -> Result<Vec<u32>, async_imap::error::Error> {
    sel(session, path).await?;
    let server: std::collections::HashSet<u32> = search_uids(session, "ALL").await?;
    let cached = cache
        .map(|c| c.cached_uids(account_id, path))
        .unwrap_or_default();
    if let Some(c) = cache {
        for uid in cached.iter() {
            if !server.contains(uid) {
                c.delete_message(account_id, path, *uid);
            }
        }
    }
    let mut remaining: Vec<u32> = server.difference(&cached).copied().collect();
    remaining.sort_unstable_by(|a, b| b.cmp(a)); // newest first
    Ok(remaining)
}

/// Fetch message summaries for a specific set of UIDs (used by the backfill).
async fn fetch_summaries_by_uid(
    account_id: u32,
    session: &mut ImapSession,
    folder_id: u32,
    path: &str,
    uids: &[u32],
    use_envelope: bool,
) -> Result<Vec<Message>, async_imap::error::Error> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    sel(session, path).await?;
    let set = uid_set(uids);
    let items: String = if use_envelope {
        format!("(UID ENVELOPE FLAGS BODYSTRUCTURE INTERNALDATE{REFS_FETCH_ITEM})")
    } else {
        "(UID FLAGS BODY.PEEK[HEADER] INTERNALDATE)".to_string()
    };
    let fetches: Vec<Fetch> = fetch_uids(session, set, items).await?.try_collect().await?;
    let mut messages: Vec<Message> = fetches
        .iter()
        .map(|f| {
            if use_envelope {
                build_summary(account_id, f, folder_id)
            } else {
                summary_from_headers(account_id, f, folder_id)
            }
        })
        .collect();
    read_gmail_labels(account_id, session, path, uids, &mut messages).await?;
    Ok(messages)
}

/// Advance one backfill job by a chunk. Fetches the next `BACKFILL_CHUNK` UIDs,
/// upserts them into the cache, and emits them as an append to the UI's index.
/// Requeues the job (at the back) if more remain. Reconnects and, if needed,
/// disables ENVELOPE parsing (iCloud) on a parse failure.
pub(super) async fn run_one_backfill(
    queue: &mut std::collections::VecDeque<Backfill>,
    session: &mut Option<ImapSession>,
    account: &AccountConfig,
    account_id: u32,
    cache: Option<&Cache>,
    prefetch: &mut std::collections::VecDeque<(String, u32)>,
    use_envelope: &mut bool,
    emit: &impl Fn(WorkerEvent),
) {
    let Some(mut job) = queue.pop_front() else {
        return;
    };
    let Some(mut s) = session.take() else {
        queue.push_front(job);
        return;
    };

    // Compute the worklist on first touch (with one reconnect on failure).
    let worklist = match job.remaining.take() {
        Some(rem) => rem,
        None => match backfill_worklist(&mut s, account_id, &job.path, cache).await {
            Ok(rem) => rem,
            Err(_) => match connect(account).await {
                Ok(fresh) => {
                    s = fresh;
                    match backfill_worklist(&mut s, account_id, &job.path, cache).await {
                        Ok(rem) => rem,
                        Err(_) => {
                            *session = Some(s);
                            queue.push_back(job);
                            return;
                        }
                    }
                }
                Err(_) => {
                    // Stay offline; retry this job on the next connect.
                    queue.push_back(job);
                    return;
                }
            },
        },
    };

    let rem = job.remaining.insert(worklist);
    if rem.is_empty() {
        emit(WorkerEvent::BackfillDone {
            folder_id: job.folder_id,
        });
        *session = Some(s); // done — don't requeue
        return;
    }
    let take = rem.len().min(BACKFILL_CHUNK);
    let chunk: Vec<u32> = rem.drain(..take).collect();

    match fetch_summaries_by_uid(
        account_id,
        &mut s,
        job.folder_id,
        &job.path,
        &chunk,
        *use_envelope,
    )
    .await
    {
        Ok(msgs) => {
            if let Some(c) = cache {
                c.upsert_messages(account_id, &job.path, &msgs);
            }
            // Gallery folders: queue this chunk's attachments for background
            // download so they appear in the attachments gallery.
            if job.gallery {
                queue_attachment_prefetch(prefetch, &job.path, &msgs, cache, account_id);
            }
            emit(WorkerEvent::MessagesAppend {
                folder_id: job.folder_id,
                messages: msgs,
            });
            *session = Some(s);
        }
        Err(e) => {
            // Put the chunk back and reconnect; a parse error means the server
            // answers this way of asking with something unreadable (iCloud's
            // ENVELOPE, Mailfence with the preview items, #226) — move down
            // the same ladder the first load uses.
            for uid in chunk.into_iter().rev() {
                rem.insert(0, uid);
            }
            if is_parse_error(&e) {
                step_fetch_mode(account_id, use_envelope);
            }
            if let Ok(fresh) = connect(account).await {
                *session = Some(fresh);
            }
            // else: session stays None; the main loop reconnects on next request.
        }
    }

    if job.remaining.as_ref().is_some_and(|r| !r.is_empty()) {
        queue.push_back(job);
    } else {
        emit(WorkerEvent::BackfillDone {
            folder_id: job.folder_id,
        });
    }
}
