import { notFound } from "next/navigation";
import Link from "next/link";
import { getPost } from "@/lib/blog-db";

export const dynamic = "force-dynamic";

export async function generateMetadata({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const post = getPost(slug);
  if (!post) return {};
  return {
    title: `${post.title} — c0mpute blog`,
    description: post.meta_description ?? undefined,
  };
}

export default async function BlogPostPage({ params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  const post = getPost(slug);
  if (!post) notFound();

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8">
      <nav className="text-sm text-[var(--color-dim)]">
        <Link href="/blog" className="!border-0 hover:text-[var(--color-accent)]">← blog</Link>
      </nav>

      <header className="space-y-3">
        <h1 className="text-2xl font-bold text-[var(--color-fg)]">{post.title}</h1>
        <time className="text-xs text-[var(--color-dim)]">
          {new Date(post.published_at).toLocaleDateString("en-US", {
            year: "numeric",
            month: "long",
            day: "numeric",
          })}
        </time>
        {post.tags.length > 0 && (
          <div className="flex flex-wrap gap-2">
            {post.tags.map((tag: string) => (
              <span
                key={tag}
                className="text-xs px-2 py-0.5 rounded border border-[var(--color-rule)] text-[var(--color-dim)]"
              >
                {tag}
              </span>
            ))}
          </div>
        )}
      </header>

      <article
        className="prose prose-invert max-w-none"
        dangerouslySetInnerHTML={{ __html: post.content_html }}
      />
    </div>
  );
}
