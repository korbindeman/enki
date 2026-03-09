import WorkerPanel from "./WorkerPanel";
import TaskList from "./TaskList";

function App() {
  return (
    <div class="flex h-screen bg-zinc-900 text-zinc-100">
      {/* Sidebar */}
      <aside class="w-[300px] shrink-0 border-r border-zinc-700 bg-zinc-950 flex flex-col">
        <div class="px-4 py-3 border-b border-zinc-700">
          <h1 class="text-lg font-semibold">Enki</h1>
        </div>
        <div class="flex-1 overflow-y-auto p-3 space-y-5">
          <WorkerPanel />
          <div class="border-t border-zinc-800" />
          <TaskList />
        </div>
      </aside>

      {/* Main content area */}
      <main class="flex-1 flex flex-col">
        <header class="p-4 border-b border-zinc-700">
          <h2 class="text-lg font-semibold">Chat</h2>
        </header>
        <div class="flex-1 overflow-y-auto p-4">
          <div class="text-zinc-500 text-sm">
            Start a conversation to begin orchestrating...
          </div>
        </div>
        <div class="p-4 border-t border-zinc-700">
          <div class="flex gap-2">
            <input
              type="text"
              placeholder="Type a message..."
              class="flex-1 rounded-lg bg-zinc-800 border border-zinc-600 px-4 py-2 text-sm text-zinc-100 placeholder-zinc-500 focus:outline-none focus:border-zinc-400"
            />
            <button class="rounded-lg bg-zinc-700 px-4 py-2 text-sm font-medium hover:bg-zinc-600 transition-colors">
              Send
            </button>
          </div>
        </div>
      </main>
    </div>
  );
}

export default App;
