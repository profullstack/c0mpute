import { ChatApp } from "./chat-app";

export const metadata = {
  title: "chat — c0mpute",
  description:
    "Chat with open models running on the c0mpute network. Sign in with CoinPay (DID required). Same chat from the browser, the c0mpute CLI, an OpenAI-compatible API and MCP.",
  alternates: { canonical: "https://c0mpute.com/chat" },
};

export const dynamic = "force-dynamic";

const ERRORS: Record<string, string> = {
  no_did:
    "Your CoinPay account has no DID yet. Create one on coinpayportal.com (Reputation, then DID), then sign in again.",
  state: "Sign-in expired or was opened in another browser. Try again.",
  signin: "CoinPay sign-in failed. Try again.",
  access_denied: "You declined the CoinPay consent.",
};

export default async function ChatPage({ searchParams }: { searchParams: Promise<{ error?: string }> }) {
  const { error } = await searchParams;
  return <ChatApp error={error ? (ERRORS[error] ?? `Sign-in failed (${error}).`) : null} />;
}
