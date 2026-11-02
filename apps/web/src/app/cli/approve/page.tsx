import { ApproveForm } from "./approve-form";

export const metadata = {
  title: "approve CLI sign-in — c0mpute",
  robots: { index: false },
};

export const dynamic = "force-dynamic";

export default async function ApprovePage({ searchParams }: { searchParams: Promise<{ code?: string }> }) {
  const { code } = await searchParams;
  return (
    <div className="max-w-xl mx-auto px-6 py-16 space-y-6 text-sm leading-7">
      <h1 className="text-2xl font-bold accent">approve CLI sign-in</h1>
      <ApproveForm initialCode={code ?? ""} />
    </div>
  );
}
