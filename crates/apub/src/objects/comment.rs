use std::ops::Deref;

use activitystreams::{object::kind::NoteType, public};
use anyhow::anyhow;
use chrono::NaiveDateTime;
use html2md::parse_html;
use url::Url;

use lemmy_api_common::blocking;
use lemmy_apub_lib::{
  traits::ApubObject,
  values::{MediaTypeHtml, MediaTypeMarkdown},
};
use lemmy_db_schema::{
  source::{
    comment::{Comment, CommentForm},
    community::Community,
    person::Person,
    post::Post,
  },
  traits::Crud,
};
use lemmy_utils::{
  utils::{convert_datetime, remove_slurs},
  LemmyError,
};
use lemmy_websocket::LemmyContext;

use crate::{
  activities::verify_person_in_community,
  fetcher::object_id::ObjectId,
  protocol::{
    objects::{
      note::{Note, SourceCompat},
      tombstone::Tombstone,
    },
    Source,
  },
  PostOrComment,
};
use lemmy_utils::utils::markdown_to_html;

#[derive(Clone, Debug)]
pub struct ApubComment(Comment);

impl Deref for ApubComment {
  type Target = Comment;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<Comment> for ApubComment {
  fn from(c: Comment) -> Self {
    ApubComment { 0: c }
  }
}

#[async_trait::async_trait(?Send)]
impl ApubObject for ApubComment {
  type DataType = LemmyContext;
  type ApubType = Note;
  type TombstoneType = Tombstone;

  fn last_refreshed_at(&self) -> Option<NaiveDateTime> {
    None
  }

  async fn read_from_apub_id(
    object_id: Url,
    context: &LemmyContext,
  ) -> Result<Option<Self>, LemmyError> {
    Ok(
      blocking(context.pool(), move |conn| {
        Comment::read_from_apub_id(conn, object_id)
      })
      .await??
      .map(Into::into),
    )
  }

  async fn delete(self, context: &LemmyContext) -> Result<(), LemmyError> {
    blocking(context.pool(), move |conn| {
      Comment::update_deleted(conn, self.id, true)
    })
    .await??;
    Ok(())
  }

  async fn to_apub(&self, context: &LemmyContext) -> Result<Note, LemmyError> {
    let creator_id = self.creator_id;
    let creator = blocking(context.pool(), move |conn| Person::read(conn, creator_id)).await??;

    let post_id = self.post_id;
    let post = blocking(context.pool(), move |conn| Post::read(conn, post_id)).await??;

    let in_reply_to = if let Some(comment_id) = self.parent_id {
      let parent_comment =
        blocking(context.pool(), move |conn| Comment::read(conn, comment_id)).await??;
      ObjectId::<PostOrComment>::new(parent_comment.ap_id.into_inner())
    } else {
      ObjectId::<PostOrComment>::new(post.ap_id.into_inner())
    };

    let note = Note {
      r#type: NoteType::Note,
      id: self.ap_id.to_owned().into_inner(),
      attributed_to: ObjectId::new(creator.actor_id),
      to: vec![public()],
      content: markdown_to_html(&self.content),
      media_type: Some(MediaTypeHtml::Html),
      source: SourceCompat::Lemmy(Source {
        content: self.content.clone(),
        media_type: MediaTypeMarkdown::Markdown,
      }),
      in_reply_to,
      published: Some(convert_datetime(self.published)),
      updated: self.updated.map(convert_datetime),
      unparsed: Default::default(),
    };

    Ok(note)
  }

  fn to_tombstone(&self) -> Result<Tombstone, LemmyError> {
    Ok(Tombstone::new(
      NoteType::Note,
      self.updated.unwrap_or(self.published),
    ))
  }

  /// Converts a `Note` to `Comment`.
  ///
  /// If the parent community, post and comment(s) are not known locally, these are also fetched.
  async fn from_apub(
    note: &Note,
    context: &LemmyContext,
    expected_domain: &Url,
    request_counter: &mut i32,
  ) -> Result<ApubComment, LemmyError> {
    let ap_id = Some(note.id(expected_domain)?.clone().into());
    let creator = note
      .attributed_to
      .dereference(context, request_counter)
      .await?;
    let (post, parent_comment_id) = note.get_parents(context, request_counter).await?;
    let community_id = post.community_id;
    let community = blocking(context.pool(), move |conn| {
      Community::read(conn, community_id)
    })
    .await??;
    verify_person_in_community(
      &note.attributed_to,
      &community.into(),
      context,
      request_counter,
    )
    .await?;
    if post.locked {
      return Err(anyhow!("Post is locked").into());
    }

    let content = if let SourceCompat::Lemmy(source) = &note.source {
      source.content.clone()
    } else {
      parse_html(&note.content)
    };
    let content_slurs_removed = remove_slurs(&content, &context.settings().slur_regex());

    let form = CommentForm {
      creator_id: creator.id,
      post_id: post.id,
      parent_id: parent_comment_id,
      content: content_slurs_removed,
      removed: None,
      read: None,
      published: note.published.map(|u| u.to_owned().naive_local()),
      updated: note.updated.map(|u| u.to_owned().naive_local()),
      deleted: None,
      ap_id,
      local: Some(false),
    };
    let comment = blocking(context.pool(), move |conn| Comment::upsert(conn, &form)).await??;
    Ok(comment.into())
  }
}

#[cfg(test)]
pub(crate) mod tests {
  use super::*;
  use crate::objects::{
    community::{tests::parse_lemmy_community, ApubCommunity},
    person::{tests::parse_lemmy_person, ApubPerson},
    post::ApubPost,
    tests::{file_to_json_object, init_context},
  };
  use assert_json_diff::assert_json_include;
  use serial_test::serial;

  async fn prepare_comment_test(
    url: &Url,
    context: &LemmyContext,
  ) -> (ApubPerson, ApubCommunity, ApubPost) {
    let person = parse_lemmy_person(context).await;
    let community = parse_lemmy_community(context).await;
    let post_json = file_to_json_object("assets/lemmy/objects/page.json");
    let post = ApubPost::from_apub(&post_json, context, url, &mut 0)
      .await
      .unwrap();
    (person, community, post)
  }

  fn cleanup(data: (ApubPerson, ApubCommunity, ApubPost), context: &LemmyContext) {
    Post::delete(&*context.pool().get().unwrap(), data.2.id).unwrap();
    Community::delete(&*context.pool().get().unwrap(), data.1.id).unwrap();
    Person::delete(&*context.pool().get().unwrap(), data.0.id).unwrap();
  }

  #[actix_rt::test]
  #[serial]
  pub(crate) async fn test_parse_lemmy_comment() {
    let context = init_context();
    let url = Url::parse("https://enterprise.lemmy.ml/comment/38741").unwrap();
    let data = prepare_comment_test(&url, &context).await;

    let json = file_to_json_object("assets/lemmy/objects/note.json");
    let mut request_counter = 0;
    let comment = ApubComment::from_apub(&json, &context, &url, &mut request_counter)
      .await
      .unwrap();

    assert_eq!(comment.ap_id.clone().into_inner(), url);
    assert_eq!(comment.content.len(), 14);
    assert!(!comment.local);
    assert_eq!(request_counter, 0);

    let to_apub = comment.to_apub(&context).await.unwrap();
    assert_json_include!(actual: json, expected: to_apub);

    Comment::delete(&*context.pool().get().unwrap(), comment.id).unwrap();
    cleanup(data, &context);
  }

  #[actix_rt::test]
  #[serial]
  async fn test_parse_pleroma_comment() {
    let context = init_context();
    let url = Url::parse("https://enterprise.lemmy.ml/comment/38741").unwrap();
    let data = prepare_comment_test(&url, &context).await;

    let pleroma_url =
      Url::parse("https://queer.hacktivis.me/objects/8d4973f4-53de-49cd-8c27-df160e16a9c2")
        .unwrap();
    let person_json = file_to_json_object("assets/pleroma/objects/person.json");
    ApubPerson::from_apub(&person_json, &context, &pleroma_url, &mut 0)
      .await
      .unwrap();
    let json = file_to_json_object("assets/pleroma/objects/note.json");
    let mut request_counter = 0;
    let comment = ApubComment::from_apub(&json, &context, &pleroma_url, &mut request_counter)
      .await
      .unwrap();

    assert_eq!(comment.ap_id.clone().into_inner(), pleroma_url);
    assert_eq!(comment.content.len(), 64);
    assert!(!comment.local);
    assert_eq!(request_counter, 0);

    Comment::delete(&*context.pool().get().unwrap(), comment.id).unwrap();
    cleanup(data, &context);
  }

  #[actix_rt::test]
  #[serial]
  async fn test_html_to_markdown_sanitize() {
    let parsed = parse_html("<script></script><b>hello</b>");
    assert_eq!(parsed, "**hello**");
  }
}
