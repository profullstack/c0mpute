import Link from "next/link";
import { listPosts } from "@/lib/blog-db";

export const metadata = { title: "blog — c0mpute" };
export const dynamic = "force-dynamic";

export default function BlogPage() {
  const posts = listPosts(50);

  return (
    <div className="max-w-3xl mx-auto px-6 py-16 space-y-8">
      <header className="space-y-2">
        <h1 className="text-2xl font-bold accent">blog</h1>
        <p className="comment">// updates from the network</p>
      </header>

      {posts.length === 0 ? (
        <p className="text-[var(--color-dim)] text-sm">no posts yet</p>
      ) : (
        <ul className="space-y-6">
          {posts.map((post) => (
            <li key={post.slug} className="border-t border-[var(--color-rule)] pt-6">
              <Link href={`/blog/${post.slug}`} className="!border-0 group block space-y-1">
                <h2 className="font-semibold text-[var(--color-fg)] group-hover:text-[var(--color-accent)] transition-colors">
                  {post.title}
                </h2>
                {post.meta_description && (
                  <p className="text-sm text-[var(--color-dim)] line-clamp-2">{post.meta_description}</p>
                )}
                <time className="text-xs text-[var(--color-dim)]">
                  {new Date(post.published_at).toLocaleDateString("en-US", {
                    year: "numeric",
                    month: "long",
                    day: "numeric",
                  })}
                </time>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
