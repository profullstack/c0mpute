import { NextRequest, NextResponse } from "next/server";

// All 3rd-party calls happen server-side so secrets never reach the browser.
export async function GET(request: NextRequest) {
  const { searchParams } = new URL(request.url);
  const query = searchParams.get("q") || "";

  try {
    // Example: proxy to a hypothetical partner API.
    // const partnerRes = await fetch("https://partner.example.com/api/data", {
    //   headers: { Authorization: `Bearer ${process.env.PARTNER_API_KEY}` },
    // });
    // const data = await partnerRes.json();

    // Stub response for now.
    const data = {
      partner: "example",
      query,
      results: [],
      fetchedAt: new Date().toISOString(),
    };

    return NextResponse.json(data);
  } catch (error) {
    return NextResponse.json(
      { error: "Partner request failed" },
      { status: 502 }
    );
  }
}

export async function POST(request: NextRequest) {
  try {
    const body = await request.json();

    // Example: forward to partner API with a secret key.
    // const partnerRes = await fetch("https://partner.example.com/api/action", {
    //   method: "POST",
    //   headers: {
    //     "Content-Type": "application/json",
    //     Authorization: `Bearer ${process.env.PARTNER_API_KEY}`,
    //   },
    //   body: JSON.stringify(body),
    // });
    // const data = await partnerRes.json();

    const data = {
      partner: "example",
      received: body,
      processedAt: new Date().toISOString(),
    };

    return NextResponse.json(data);
  } catch (error) {
    return NextResponse.json(
      { error: "Partner request failed" },
      { status: 502 }
    );
  }
}
