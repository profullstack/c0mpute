import { AdminList } from "./admin-list";

export const metadata = { title: "gallery moderation — c0mpute", robots: { index: false } };

export default function GalleryAdminPage() {
  return (
    <div className="max-w-6xl mx-auto px-4 sm:px-6 py-12 space-y-6 text-sm">
      <header className="space-y-1">
        <h1 className="text-2xl font-bold accent">gallery moderation</h1>
        <p className="comment">// approve, reject (with a refund flag for paid ones), feature. Moderators only: sign in at /chat first.</p>
      </header>
      <AdminList />
    </div>
  );
}
