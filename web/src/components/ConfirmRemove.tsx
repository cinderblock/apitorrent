import { useEffect, useRef, useState } from "react";

interface Props {
  count: number;
  onConfirm: (deleteFiles: boolean) => void;
  onClose: () => void;
}

export function ConfirmRemove({ count, onConfirm, onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [deleteFiles, setDeleteFiles] = useState(false);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const what = count === 1 ? "this torrent" : `these ${count} torrents`;
  return (
    <dialog ref={dialog} className="dialog" onClose={onClose} aria-labelledby="remove-title">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          onConfirm(deleteFiles);
        }}
      >
        <h2 id="remove-title">Remove {what}?</h2>
        <label className="check">
          <input type="checkbox" checked={deleteFiles} onChange={(e) => setDeleteFiles(e.target.checked)} />
          Also delete the downloaded files
        </label>
        <div className="actions">
          <button type="button" onClick={onClose}>Cancel</button>
          <button type="submit" className={deleteFiles ? "danger" : "primary"}>
            {deleteFiles ? "Remove and delete files" : "Remove"}
          </button>
        </div>
      </form>
    </dialog>
  );
}
