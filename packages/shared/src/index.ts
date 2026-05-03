export interface User {
  id: string;
  email: string;
  createdAt: string;
}

export interface ApiResponse<T> {
  data?: T;
  error?: string;
}

export const API_VERSION = "v1";
