require 'front_matter_parser'

class Post
  include Comparable

  attr_reader :author_id,
    :date,
    :description,
    :id,
    :path,
    :permalink,
    :tags,
    :title

  def initialize(path)
    path_parts = path.split("-", 3)

    @date = Date.parse("#{path_parts.fetch(0)}-#{path_parts.fetch(1)}-#{path_parts.fetch(2)}")
    @path = Pathname.new(path).relative_path_from(ROOT_DIR).to_s

    parsed = FrontMatterParser::Parser.parse_file(path)
    front_matter = parsed.front_matter

    @author_id = front_matter.fetch("author_id")
    @description = parsed.content.split("\n\n").first.remove_markdown_links
    @id = front_matter.fetch("id")
    @permalink = "#{BLOG_HOST}/#{id}"
    @tags = front_matter.fetch("tags")
    @title = front_matter.fetch("title")
  end

  def <=>(other)
    date <=> other.date
  end

  def eql?(other)
    self.<=>(other) == 0
  end

  def type?(name)
    tags.any? { |tag| tag == "type: announcement" }
  end

  def to_h
    {
      author_id: author_id,
      date: date,
      description: description,
      id: id,
      path: path,
      permalink: permalink,
      tags: tags,
      title: title
    }
  end
end