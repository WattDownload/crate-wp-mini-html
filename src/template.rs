use askama::Template;

pub struct StoryPart {
    pub title: String,
    pub content: String,
}

#[derive(Template)]
#[template(path = "template.html")]
pub struct StoryTemplate<'a> {
    pub title: &'a str,
    pub author: &'a str,
    pub published: &'a str,
    pub description: &'a str,
    pub cover: &'a str,
    pub avatar: &'a str,
    pub story_id: &'a str,
    pub no_parts: usize,
    pub lang: &'a str,
    pub direction: &'a str,
    pub parts: Vec<StoryPart>, 
}