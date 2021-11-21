from fastapi import APIRouter, Depends
from fastapi.responses import Response

from app.services.dowloaders_manager import DownloadersManager

from app.depends import check_token


router = APIRouter(
    tags=["downloader"],
    dependencies=[Depends(check_token)],
)


@router.get("/download/{source_id}/{remote_id}/{file_type}")
async def download(source_id: int, remote_id: int, file_type: str):
    downloader = await DownloadersManager.get_downloader(source_id)

    content, filename = await downloader.download(remote_id, file_type, source_id)

    return Response(
        content,
        headers={
            "Conten-Disposition": f"attachment; filename={filename}"
        }
    )
