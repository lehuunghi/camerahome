"""Low-resolution motion detector; separate from HD archive capture."""
import cv2
import numpy as np


class MotionDetector:
    def __init__(self):
        self.background=None
        self.frames=0
        self.consecutive=0

    def feed(self, frame):
        gray=cv2.cvtColor(frame,cv2.COLOR_BGR2GRAY) if frame.ndim==3 else frame
        blur=cv2.GaussianBlur(gray,(5,5),0)
        self.frames+=1
        if self.background is None:
            self.background=blur.astype(np.float32)
            return False,0.
        difference=cv2.absdiff(blur,cv2.convertScaleAbs(self.background))
        difference=cv2.subtract(difference,np.full_like(difference,int(np.median(difference))))
        mask=cv2.threshold(difference,22,255,cv2.THRESH_BINARY)[1]
        mask=cv2.morphologyEx(mask,cv2.MORPH_OPEN,np.ones((3,3),np.uint8))
        contours,_=cv2.findContours(mask,cv2.RETR_EXTERNAL,cv2.CHAIN_APPROX_SIMPLE)
        area=max((cv2.contourArea(c) for c in contours),default=0)
        self.consecutive=self.consecutive+1 if area>140 and self.frames>15 else 0
        triggered=self.consecutive>=2
        if triggered:self.consecutive=0
        cv2.accumulateWeighted(blur,self.background,.04)
        return triggered,area/gray.size
